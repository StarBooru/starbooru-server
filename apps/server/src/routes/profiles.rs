//! Profiles: anyone's by username on a public instance (guests included),
//! and the caller's own with its picture. A private instance keeps display
//! names only, and shows nobody's profile to others.

use axum::Json;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use sqlx::FromRow;
use starbooru_core::{
    AvatarCrop, Mode, Profile, ProfileUpdate, Role, clean_about, clean_display_name,
};

use crate::auth::AuthUser;
use crate::avatars;
use crate::error::{AppError, AppResult};
use crate::guests::Viewer;
use crate::settings;
use crate::state::AppState;

/// A profile with what the account has done, for `WHERE` to pick.
macro_rules! profile_sql {
    ($where:literal) => {
        concat!(
            "SELECT u.username, u.role, u.created_at, u.display_name, u.about, u.avatar_sha, \
                 (SELECT count(*) FROM posts p \
                  WHERE p.uploader_id = u.id AND p.deleted_at IS NULL) AS posts, \
                 (SELECT count(*) FROM forum_posts f \
                  WHERE f.author_id = u.id AND f.deleted_at IS NULL) AS forum_posts \
             FROM users u WHERE ",
            $where
        )
    };
}

#[derive(FromRow)]
struct ProfileRow {
    username: String,
    role: String,
    created_at: DateTime<Utc>,
    display_name: Option<String>,
    about: Option<String>,
    avatar_sha: Option<Vec<u8>>,
    posts: i64,
    forum_posts: i64,
}

fn to_profile(state: &AppState, row: ProfileRow) -> AppResult<Profile> {
    let (avatar_url, avatar_small_url) = row
        .avatar_sha
        .map(|sha| avatars::urls(state, &hex::encode(sha)))
        .unzip();
    Ok(Profile {
        name: row.username,
        display_name: row.display_name,
        about: row.about,
        avatar_url,
        avatar_small_url,
        role: Role::from_db(&row.role).ok_or_else(|| AppError::internal("bad role in db"))?,
        joined: row.created_at,
        posts: u64::try_from(row.posts).unwrap_or(0),
        forum_posts: u64::try_from(row.forum_posts).unwrap_or(0),
    })
}

pub(crate) const OFF: &str = "profiles_off";
pub(crate) const OFF_MESSAGE: &str = "a private server shows nobody's profile to others";

/// What a private instance answers for what only public ones show.
fn profiles_off() -> AppError {
    AppError::Refused {
        code: OFF,
        message: OFF_MESSAGE,
    }
}

async fn public(state: &AppState) -> AppResult<bool> {
    Ok(settings::mode(state).await? == Mode::Public)
}

async fn own(state: &AppState, user_id: i64) -> AppResult<Profile> {
    let row: ProfileRow = sqlx::query_as(profile_sql!("u.id = $1"))
        .bind(user_id)
        .fetch_one(&state.db)
        .await?;
    to_profile(state, row)
}

/// `GET /users/{name}`: anyone's profile on a public instance.
pub async fn get(
    State(state): State<AppState>,
    _viewer: Viewer,
    Path(name): Path<String>,
) -> AppResult<Json<Profile>> {
    if !public(&state).await? {
        return Err(profiles_off());
    }
    let row: ProfileRow = sqlx::query_as(profile_sql!("lower(u.username) = lower($1)"))
        .bind(name.trim())
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    to_profile(&state, row).map(Json)
}

/// `GET /auth/profile`: the caller's own, in either mode.
pub async fn mine(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<Profile>> {
    own(&state, user.id).await.map(Json)
}

/// `PATCH /auth/profile`: the display name in either mode, the text about
/// the account on a public instance. Clearing either always works.
pub async fn update(
    State(state): State<AppState>,
    user: AuthUser,
    Json(req): Json<ProfileUpdate>,
) -> AppResult<Json<Profile>> {
    let display_name = req
        .display_name
        .map(|name| clean_display_name(name.as_deref().unwrap_or_default()))
        .transpose()?;
    let about = req
        .about
        .map(|text| clean_about(text.as_deref().unwrap_or_default()))
        .transpose()?;
    let mut tx = state.db.begin().await?;
    if about.as_ref().is_some_and(Option::is_some) {
        // Held public until saved, so a switch to private can't miss it.
        crate::modes::hold_public(&mut tx, OFF, OFF_MESSAGE).await?;
    }
    sqlx::query(
        "UPDATE users SET \
             display_name = CASE WHEN $2 THEN $3 ELSE display_name END, \
             about = CASE WHEN $4 THEN $5 ELSE about END \
         WHERE id = $1",
    )
    .bind(user.id)
    .bind(display_name.is_some())
    .bind(display_name.flatten())
    .bind(about.is_some())
    .bind(about.flatten())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    own(&state, user.id).await.map(Json)
}

/// `PUT /auth/avatar?x=&y=&size=`: the square to cut, all three or none.
#[derive(Deserialize)]
pub struct CropQuery {
    x: Option<u32>,
    y: Option<u32>,
    size: Option<u32>,
}

impl CropQuery {
    fn crop(&self) -> AppResult<Option<AvatarCrop>> {
        match (self.x, self.y, self.size) {
            (None, None, None) => Ok(None),
            (Some(x), Some(y), Some(size)) => Ok(Some(AvatarCrop { x, y, size })),
            _ => Err(AppError::BadRequest(
                "give the square's x, y and size together, or none for the middle".into(),
            )),
        }
    }
}

/// `PUT /auth/avatar`: the body is the picture (JPEG, PNG, WebP or GIF).
/// Public instances only.
pub async fn put_avatar(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<CropQuery>,
    body: Result<Bytes, BytesRejection>,
) -> AppResult<Json<Profile>> {
    let body = body.map_err(|e| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            AppError::PayloadTooLarge
        } else {
            AppError::BadRequest(e.body_text())
        }
    })?;
    if !public(&state).await? {
        return Err(profiles_off());
    }
    let crop = q.crop()?;
    if body.is_empty() {
        return Err(AppError::BadRequest(
            "send the picture as the request's body".into(),
        ));
    }
    let made = tokio::task::spawn_blocking(move || avatars::make(&body, crop)).await??;
    avatars::set(&state, user.id, made, (OFF, OFF_MESSAGE)).await?;
    own(&state, user.id).await.map(Json)
}

/// `DELETE /auth/avatar`: in either mode.
pub async fn delete_avatar(
    State(state): State<AppState>,
    user: AuthUser,
) -> AppResult<Json<Profile>> {
    avatars::clear(&state, user.id).await?;
    own(&state, user.id).await.map(Json)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use axum::http::{Method, StatusCode};
    use serde_json::{Value, json};

    use crate::testing::{self, TestApp};

    fn picture(w: u32, h: u32, format: image::ImageFormat) -> Vec<u8> {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        }));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    async fn patch(app: &TestApp, token: &str, body: Value) -> testing::Resp {
        app.call(
            Method::PATCH,
            "/api/v1/auth/profile",
            Some(token),
            Some(body),
        )
        .await
    }

    /// The side of the JPEG at a signed media URL.
    async fn side(app: &TestApp, url: &str) -> u32 {
        let r = app.call(Method::GET, url, None, None).await;
        assert_eq!(r.status, StatusCode::OK, "{url}");
        let img = crate::storage::decode(&r.body).unwrap();
        assert_eq!(img.width(), img.height());
        img.width()
    }

    fn avatar_files(app: &TestApp) -> Vec<String> {
        let mut names = app.state.storage.avatar_names().unwrap();
        names.sort();
        names
    }

    #[tokio::test]
    async fn profiles_show_on_a_public_server_only() {
        let Some(app) = testing::app().await else {
            return;
        };
        let admin = app.register("admin").await;
        let ann = app.register("ann").await;
        let png = picture(400, 300, image::ImageFormat::Png);

        // Private: your own profile and display name, nothing for others.
        let r = patch(&app, &ann, json!({ "display_name": "  Ann   B " })).await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        assert_eq!(r.json()["display_name"], "Ann B");
        assert_eq!(r.json()["name"], "ann");
        for (method, path, body) in [
            (
                Method::PATCH,
                "/api/v1/auth/profile",
                Some(json!({ "about": "hi" })),
            ),
            (Method::GET, "/api/v1/users/ann", None),
        ] {
            let r = app.call(method, path, Some(&admin), body).await;
            assert_eq!(
                (r.status, r.json()["code"].clone()),
                (StatusCode::FORBIDDEN, json!("profiles_off")),
                "{path}"
            );
        }
        let r = app
            .put_bytes("/api/v1/auth/avatar", Some(&ann), png.clone())
            .await;
        assert_eq!(r.json()["code"], "profiles_off");
        let r = app.call(Method::GET, "/api/v1/users/ann", None, None).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        let post = app.upload_png(&ann, 1, "sky").await;
        let p = app
            .get(&format!("/api/v1/posts/{post}"), &admin)
            .await
            .json();
        assert!(p.get("uploader").is_none(), "{p}");

        // Public: a text, a picture, and everyone sees them.
        app.switch(&admin, "public").await;
        let r = patch(
            &app,
            &ann,
            json!({ "about": "I tag skies.\r\nAnd cats.", "display_name": "Ann" }),
        )
        .await;
        assert_eq!(r.json()["about"], "I tag skies.\nAnd cats.");
        for bad in [
            json!({ "display_name": "x".repeat(41) }),
            json!({ "display_name": "a\u{202e}b" }),
            json!({ "about": "y".repeat(501) }),
        ] {
            assert_eq!(patch(&app, &ann, bad.clone()).await.status, 400, "{bad}");
        }
        let r = app
            .put_bytes("/api/v1/auth/avatar", Some(&ann), png.clone())
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        let me = r.json();
        assert_eq!(side(&app, me["avatar_url"].as_str().unwrap()).await, 256);
        assert_eq!(
            side(&app, me["avatar_small_url"].as_str().unwrap()).await,
            64
        );
        assert!(
            me["avatar_url"]
                .as_str()
                .unwrap()
                .starts_with("/media/avatar/")
        );
        assert_eq!(avatar_files(&app).len(), 1);

        let binned = app.upload_png(&ann, 2, "sky").await;
        app.call(
            Method::DELETE,
            &format!("/api/v1/posts/{binned}"),
            Some(&ann),
            None,
        )
        .await;
        let r = app.call(Method::GET, "/api/v1/users/ANN", None, None).await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        let seen = r.json();
        assert_eq!(
            (
                &seen["name"],
                &seen["display_name"],
                &seen["role"],
                &seen["posts"]
            ),
            (&json!("ann"), &json!("Ann"), &json!("user"), &json!(1))
        );
        assert_eq!(seen["forum_posts"], 0);
        assert_eq!(seen["avatar_url"], me["avatar_url"]);
        assert!(seen["joined"].is_string());
        let r = app
            .call(Method::GET, "/api/v1/users/nobody", None, None)
            .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);

        // Posts say who uploaded them, guests included.
        let p = app
            .call(Method::GET, &format!("/api/v1/posts/{post}"), None, None)
            .await
            .json();
        assert_eq!(p["uploader"]["name"], "ann");
        assert_eq!(p["uploader"]["display_name"], "Ann");
        assert_eq!(p["uploader"]["avatar_small_url"], me["avatar_small_url"]);
        let page = app
            .call(Method::GET, "/api/v1/posts", None, None)
            .await
            .json();
        assert_eq!(page["posts"][0]["uploader"]["name"], "ann");

        // Cut where asked; refused when it doesn't fit or isn't a picture.
        let r = app
            .put_bytes(
                "/api/v1/auth/avatar?x=100&y=50&size=250",
                Some(&ann),
                png.clone(),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        assert_ne!(r.json()["avatar_url"], me["avatar_url"]);
        assert_eq!(avatar_files(&app).len(), 1, "the old one went");
        for (path, body, status) in [
            ("/api/v1/auth/avatar?x=200&y=0&size=250", png.clone(), 400),
            ("/api/v1/auth/avatar?x=1", png.clone(), 400),
            ("/api/v1/auth/avatar", b"not a picture".to_vec(), 415),
            (
                "/api/v1/auth/avatar",
                picture(40, 40, image::ImageFormat::Bmp),
                415,
            ),
            ("/api/v1/auth/avatar", Vec::new(), 400),
            (
                "/api/v1/auth/avatar",
                vec![0u8; starbooru_core::profile::MAX_AVATAR_BYTES + 1],
                413,
            ),
        ] {
            let r = app.put_bytes(path, Some(&ann), body).await;
            assert_eq!(r.status.as_u16(), status, "{path}: {:?}", r.json());
        }
        let r = app
            .put_bytes("/api/v1/auth/avatar", None, png.clone())
            .await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        let gif = picture(64, 80, image::ImageFormat::Gif);
        let r = app
            .put_bytes("/api/v1/auth/avatar", Some(&admin), gif)
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());

        // The same picture twice is one pair of files, kept while one uses it.
        let r = app
            .put_bytes("/api/v1/auth/avatar", Some(&admin), png.clone())
            .await;
        let shared = r.json()["avatar_url"].clone();
        let r = app
            .put_bytes("/api/v1/auth/avatar", Some(&ann), png.clone())
            .await;
        assert_eq!(r.json()["avatar_url"], shared);
        assert_eq!(avatar_files(&app).len(), 1);
        let r = app
            .call(Method::DELETE, "/api/v1/auth/avatar", Some(&ann), None)
            .await;
        assert!(r.json()["avatar_url"].is_null());
        assert_eq!(avatar_files(&app).len(), 1);

        // Admins clear what others shouldn't see, their own included.
        app.put_bytes("/api/v1/auth/avatar", Some(&ann), png).await;
        let users = app.get("/api/v1/admin/users", &admin).await.json();
        let ann_row = &users[1];
        assert_eq!(ann_row["display_name"], "Ann");
        assert!(
            ann_row["avatar_url"]
                .as_str()
                .unwrap()
                .starts_with("/media/avatar64/")
        );
        let (ann_id, admin_id) = (
            ann_row["id"].as_i64().unwrap(),
            users[0]["id"].as_i64().unwrap(),
        );
        let r = app
            .call(
                Method::PATCH,
                &format!("/api/v1/admin/users/{ann_id}"),
                Some(&admin),
                Some(json!({ "clear_avatar": true, "clear_about": true, "clear_display_name": true })),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        assert!(r.json().get("avatar_url").is_none() && r.json().get("about").is_none());
        let seen = app.get("/api/v1/users/ann", &admin).await.json();
        assert_eq!(
            (&seen["avatar_url"], &seen["about"], &seen["display_name"]),
            (&Value::Null, &Value::Null, &Value::Null)
        );
        let own = format!("/api/v1/admin/users/{admin_id}");
        let r = app
            .call(
                Method::PATCH,
                &own,
                Some(&admin),
                Some(json!({ "clear_avatar": true })),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        assert!(avatar_files(&app).is_empty());
        let r = app
            .call(
                Method::PATCH,
                &own,
                Some(&admin),
                Some(json!({ "clear_about": true, "role": "user" })),
            )
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        let r = app
            .call(
                Method::PATCH,
                &format!("/api/v1/admin/users/{admin_id}"),
                Some(&ann),
                Some(json!({ "clear_avatar": true })),
            )
            .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN);
        app.finish().await;
    }

    #[tokio::test]
    async fn a_deleted_account_takes_its_picture_with_it() {
        let Some(app) = testing::app_with(|c| c.mode = Some(starbooru_core::Mode::Public)).await
        else {
            return;
        };
        let admin = app.register("admin").await;
        let bob = app.register("bob").await;
        let r = app
            .put_bytes(
                "/api/v1/auth/avatar",
                Some(&bob),
                picture(50, 50, image::ImageFormat::Jpeg),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        assert_eq!(avatar_files(&app).len(), 1);
        let id = app.get("/api/v1/admin/users", &admin).await.json()[1]["id"]
            .as_i64()
            .unwrap();
        app.call(
            Method::DELETE,
            &format!("/api/v1/admin/users/{id}"),
            Some(&admin),
            None,
        )
        .await;
        assert!(avatar_files(&app).is_empty());
        app.finish().await;
    }
}
