//! Votes on a post's tags, on a public instance, and the tags they took off
//! (`api.md` › Tag votes). A tag voted down to the threshold comes off its
//! post at once, its votes there are cleared and it is recorded in
//! `tag_removals`; while the record is there, edits leave the tag off
//! (`posts::set_tags`). An admin's undo puts it back to start over.

use axum::Json;
use axum::extract::{Path, Query, State};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use starbooru_core::{
    DEFAULT_PAGE_LIMIT, MAX_PAGE_LIMIT, Post, TagRemoval, TagRemovalPage, TagVoteRequest, voted_off,
};

use super::admin::require_admin_session;
use super::posts::{load_full, lock_live, media_urls, set_tags};
use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::{modes, settings};

const MAX_PAGE: u32 = 10_000;

/// `PUT /posts/{id}/tags/{name}/vote`: 1, -1, or 0 to take the vote back.
pub async fn vote(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, name)): Path<(i64, String)>,
    Json(req): Json<TagVoteRequest>,
) -> AppResult<Json<Post>> {
    if !(-1..=1).contains(&req.value) {
        return Err(AppError::BadRequest(
            "a vote on a tag is 1, -1, or 0 to take it back".into(),
        ));
    }
    let mut tx = state.db.begin().await?;
    // A switch to private deletes the votes; this one can't land after it.
    modes::hold_public(
        &mut tx,
        "tag_votes_off",
        "a private server doesn't vote on tags",
    )
    .await?;
    lock_live(&mut tx, id).await?;
    let (tag_id, tag, ..) = super::tags::find(&mut *tx, &name).await?;
    let on_post: bool = sqlx::query_scalar("SELECT tag_ids @> ARRAY[$2] FROM posts WHERE id = $1")
        .bind(id)
        .bind(tag_id)
        .fetch_one(&mut *tx)
        .await?;
    if !on_post {
        return Err(AppError::BadRequest(format!("`{tag}` isn't on this post")));
    }
    if req.value == 0 {
        sqlx::query("DELETE FROM tag_votes WHERE post_id = $1 AND tag_id = $2 AND user_id = $3")
            .bind(id)
            .bind(tag_id)
            .bind(user.id)
            .execute(&mut *tx)
            .await?;
    } else {
        sqlx::query(
            "INSERT INTO tag_votes (post_id, tag_id, user_id, value) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (post_id, tag_id, user_id) DO UPDATE SET value = EXCLUDED.value, \
             voted_at = now()",
        )
        .bind(id)
        .bind(tag_id)
        .bind(user.id)
        .bind(req.value)
        .execute(&mut *tx)
        .await?;
    }
    if req.value < 0 {
        let score: i64 = sqlx::query_scalar(
            "SELECT COALESCE(sum(value), 0)::bigint FROM tag_votes \
             WHERE post_id = $1 AND tag_id = $2",
        )
        .bind(id)
        .bind(tag_id)
        .fetch_one(&mut *tx)
        .await?;
        let threshold = settings::tag_vote_threshold(&mut *tx).await?;
        if voted_off(score, threshold) {
            // Recorded first, so `set_tags` keeps it off even when another
            // tag implies it.
            sqlx::query(
                "INSERT INTO tag_removals (post_id, tag_id, score) VALUES ($1, $2, $3) \
                 ON CONFLICT (post_id, tag_id) DO UPDATE SET score = EXCLUDED.score, \
                 removed_at = now()",
            )
            .bind(id)
            .bind(tag_id)
            .bind(i32::try_from(score).unwrap_or(i32::MIN))
            .execute(&mut *tx)
            .await?;
            // Its votes have done their part; the record keeps the score.
            sqlx::query("DELETE FROM tag_votes WHERE post_id = $1 AND tag_id = $2")
                .bind(id)
                .bind(tag_id)
                .execute(&mut *tx)
                .await?;
            let rest = other_tags(&mut tx, id, tag_id).await?;
            set_tags(&mut tx, id, &rest).await?;
        }
    }
    tx.commit().await?;
    load_full(&state, user.id, id).await.map(Json)
}

/// The names of a post's tags but one.
async fn other_tags(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    post_id: i64,
    but: i64,
) -> AppResult<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT t.name FROM posts p JOIN tags t ON t.id = ANY(p.tag_ids) \
         WHERE p.id = $1 AND t.id <> $2",
    )
    .bind(post_id)
    .bind(but)
    .fetch_all(&mut **tx)
    .await?)
}

#[derive(Deserialize)]
pub struct PageQuery {
    page: Option<u32>,
    limit: Option<u32>,
}

#[derive(sqlx::FromRow)]
struct RemovalRow {
    id: i64,
    post_id: i64,
    tag: String,
    score: i32,
    removed_at: DateTime<Utc>,
    sha256: Vec<u8>,
    ext: String,
    has_thumb: bool,
}

/// `GET /admin/tag-removals?page=&limit=`: tags the votes took off posts,
/// newest first.
pub async fn removals(
    State(state): State<AppState>,
    user: AuthUser,
    Query(params): Query<PageQuery>,
) -> AppResult<Json<TagRemovalPage>> {
    user.require_admin()?;
    let page = params.page.unwrap_or(1).clamp(1, MAX_PAGE);
    let limit = params
        .limit
        .unwrap_or(DEFAULT_PAGE_LIMIT)
        .clamp(1, MAX_PAGE_LIMIT);
    let mut rows: Vec<RemovalRow> = sqlx::query_as(
        "SELECT r.id, r.post_id, t.name AS tag, r.score, r.removed_at, \
                p.sha256, p.ext, p.has_thumb \
         FROM tag_removals r JOIN tags t ON t.id = r.tag_id JOIN posts p ON p.id = r.post_id \
         ORDER BY r.removed_at DESC, r.id DESC LIMIT $1 OFFSET $2",
    )
    .bind(i64::from(limit) + 1)
    .bind(i64::from(page - 1) * i64::from(limit))
    .fetch_all(&state.db)
    .await?;
    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let removals = rows
        .into_iter()
        .map(|r| TagRemoval {
            id: r.id,
            post_id: r.post_id,
            tag: r.tag,
            score: r.score,
            removed_at: r.removed_at,
            thumb_url: media_urls(&state, &hex::encode(&r.sha256), &r.ext, r.has_thumb).1,
        })
        .collect();
    Ok(Json(TagRemovalPage {
        removals,
        page,
        limit,
        has_more,
    }))
}

/// `DELETE /admin/tag-removals/{id}`: puts the tag back on its post, with
/// no votes.
pub async fn undo(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> AppResult<Json<Post>> {
    require_admin_session(&user)?;
    let mut tx = state.db.begin().await?;
    let (post_id, tag_id, tag): (i64, i64, String) = sqlx::query_as(
        "SELECT r.post_id, r.tag_id, t.name FROM tag_removals r JOIN tags t ON t.id = r.tag_id \
         WHERE r.id = $1",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    lock_live(&mut tx, post_id).await?;
    sqlx::query("DELETE FROM tag_removals WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    // None are left from the removal, but an alias merge may bring some.
    sqlx::query("DELETE FROM tag_votes WHERE post_id = $1 AND tag_id = $2")
        .bind(post_id)
        .bind(tag_id)
        .execute(&mut *tx)
        .await?;
    let mut names = other_tags(&mut tx, post_id, tag_id).await?;
    names.push(tag);
    set_tags(&mut tx, post_id, &names).await?;
    tx.commit().await?;
    load_full(&state, user.id, post_id).await.map(Json)
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use serde_json::{Value, json};

    use crate::testing::{self, TestApp};

    async fn vote(app: &TestApp, token: &str, post: i64, tag: &str, value: i16) -> testing::Resp {
        app.call(
            Method::PUT,
            &format!("/api/v1/posts/{post}/tags/{tag}/vote"),
            Some(token),
            Some(json!({ "value": value })),
        )
        .await
    }

    fn tags(post: &Value) -> Vec<String> {
        post["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn votes_take_a_tag_off_and_an_admin_puts_it_back() {
        let Some(app) = testing::app().await else {
            return;
        };
        let admin = app.register("admin").await;
        let voters = [
            app.register("ann").await,
            app.register("bea").await,
            app.register("cid").await,
        ];
        let p = app.upload_png(&admin, 1, "sky cat").await;
        let other = app.upload_png(&admin, 2, "cat dog").await;

        // Private servers don't vote on tags.
        let r = vote(&app, &voters[0], p, "cat", -1).await;
        assert_eq!(
            (r.status, r.json()["code"].clone()),
            (StatusCode::FORBIDDEN, json!("tag_votes_off"))
        );
        app.switch(&admin, "public").await;
        let settings = app.get("/api/v1/admin/settings", &admin).await.json();
        assert_eq!(settings["tag_vote_threshold"], 3);

        // Each account has one vote a tag; the post shows the sum and yours.
        let r = vote(&app, &voters[0], p, "cat", 1).await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        assert_eq!(
            r.json()["tag_votes"]["cat"],
            json!({ "score": 1, "mine": 1 })
        );
        assert!(r.json()["tag_votes"].get("sky").is_none());
        let r = vote(&app, &voters[0], p, "cat", -1).await;
        assert_eq!(
            r.json()["tag_votes"]["cat"],
            json!({ "score": -1, "mine": -1 })
        );
        let seen = app
            .get(&format!("/api/v1/posts/{p}"), &voters[1])
            .await
            .json();
        assert_eq!(seen["tag_votes"]["cat"], json!({ "score": -1, "mine": 0 }));
        let page = app
            .call(Method::GET, "/api/v1/posts?q=sky", None, None)
            .await
            .json();
        assert_eq!(page["posts"][0]["tag_votes"]["cat"]["score"], -1);
        let r = vote(&app, &voters[0], p, "cat", 0).await;
        assert!(r.json().get("tag_votes").is_none());
        for (tag, value, status) in [("cat", 2, 400), ("dog", 1, 400), ("nothing_here", 1, 404)] {
            let r = vote(&app, &voters[0], p, tag, value).await;
            assert_eq!(r.status.as_u16(), status, "{tag}: {:?}", r.json());
        }
        let r = app
            .call(
                Method::PUT,
                &format!("/api/v1/posts/{p}/tags/cat/vote"),
                None,
                Some(json!({ "value": 1 })),
            )
            .await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);

        // At -3 it comes off this post only, and edits leave it off.
        vote(&app, &voters[0], p, "cat", -1).await;
        vote(&app, &voters[1], p, "cat", -1).await;
        let r = vote(&app, &voters[2], p, "cat", -1).await;
        assert_eq!(tags(&r.json()), ["sky"]);
        assert_eq!(r.json()["voted_off"], json!(["cat"]));
        assert_eq!(
            tags(
                &app.get(&format!("/api/v1/posts/{other}"), &admin)
                    .await
                    .json()
            ),
            ["cat", "dog"]
        );
        let r = app
            .call(
                Method::PATCH,
                &format!("/api/v1/posts/{p}"),
                Some(&admin),
                Some(json!({ "tags": ["sky", "cat", "blue"] })),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        assert_eq!(tags(&r.json()), ["blue", "sky"]);
        let r = vote(&app, &voters[0], p, "cat", 1).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "it isn't on the post");

        // The admin's list, then the undo.
        let r = app.get("/api/v1/admin/tag-removals", &voters[0]).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN);
        let list = app.get("/api/v1/admin/tag-removals", &admin).await.json();
        let removal = &list["removals"][0];
        assert_eq!(
            (&removal["post_id"], &removal["tag"], &removal["score"]),
            (&json!(p), &json!("cat"), &json!(-3))
        );
        assert!(
            removal["thumb_url"]
                .as_str()
                .unwrap()
                .starts_with("/media/thumb/")
        );
        assert_eq!(list["has_more"], false);
        let undo = format!("/api/v1/admin/tag-removals/{}", removal["id"]);
        let r = app.call(Method::DELETE, &undo, Some(&admin), None).await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        assert_eq!(tags(&r.json()), ["blue", "cat", "sky"]);
        assert!(r.json().get("voted_off").is_none());
        assert!(r.json().get("tag_votes").is_none(), "its votes start over");
        let r = app.call(Method::DELETE, &undo, Some(&admin), None).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);

        // The threshold is the admin's: 0 never takes one off.
        let mut s = app.get("/api/v1/admin/settings", &admin).await.json();
        s["tag_vote_threshold"] = json!(0);
        let r = app
            .call(
                Method::PUT,
                "/api/v1/admin/settings",
                Some(&admin),
                Some(s.clone()),
            )
            .await;
        assert_eq!(r.json()["tag_vote_threshold"], 0);
        for v in &voters {
            vote(&app, v, p, "sky", -1).await;
        }
        let r = vote(&app, &admin, p, "sky", -1).await;
        assert_eq!(r.json()["tag_votes"]["sky"]["score"], -4);
        assert!(tags(&r.json()).contains(&"sky".to_owned()));
        s["tag_vote_threshold"] = json!(101);
        let r = app
            .call(
                Method::PUT,
                "/api/v1/admin/settings",
                Some(&admin),
                Some(s.clone()),
            )
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        // An old app's save leaves it as it is.
        s.as_object_mut().unwrap().remove("tag_vote_threshold");
        let r = app
            .call(Method::PUT, "/api/v1/admin/settings", Some(&admin), Some(s))
            .await;
        assert_eq!(r.json()["tag_vote_threshold"], 0);
        app.finish().await;
    }

    #[tokio::test]
    async fn an_implied_tag_voted_off_stays_off() {
        let Some(app) = testing::app_with(|c| c.mode = Some(starbooru_core::Mode::Public)).await
        else {
            return;
        };
        let admin = app.register("admin").await;
        app.call(
            Method::POST,
            "/api/v1/tags/kitten/implications",
            Some(&admin),
            Some(json!({ "implies": "cat" })),
        )
        .await;
        let p = app.upload_png(&admin, 1, "kitten").await;
        assert_eq!(
            tags(&app.get(&format!("/api/v1/posts/{p}"), &admin).await.json()),
            ["cat", "kitten"]
        );
        let mut s = app.get("/api/v1/admin/settings", &admin).await.json();
        s["tag_vote_threshold"] = json!(1);
        app.call(Method::PUT, "/api/v1/admin/settings", Some(&admin), Some(s))
            .await;
        let r = vote(&app, &admin, p, "cat", -1).await;
        assert_eq!(tags(&r.json()), ["kitten"]);
        let post = |id: i64| {
            let (app, admin) = (&app, admin.clone());
            async move { tags(&app.get(&format!("/api/v1/posts/{id}"), &admin).await.json()) }
        };

        // An implication added again leaves it off too.
        app.call(
            Method::DELETE,
            "/api/v1/tags/kitten/implications/cat",
            Some(&admin),
            None,
        )
        .await;
        let r = app
            .call(
                Method::POST,
                "/api/v1/tags/kitten/implications",
                Some(&admin),
                Some(json!({ "implies": "cat" })),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        assert_eq!(post(p).await, ["kitten"]);

        // So does a tag merged into it.
        let q = app.upload_png(&admin, 2, "sky kitty cat").await;
        vote(&app, &admin, q, "cat", -1).await;
        assert_eq!(post(q).await, ["kitty", "sky"]);
        let r = app
            .call(
                Method::POST,
                "/api/v1/tags/cat/aliases",
                Some(&admin),
                Some(json!({ "alias": "kitty" })),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        assert_eq!(post(q).await, ["sky"]);
        let list = app.get("/api/v1/admin/tag-removals", &admin).await.json();
        assert_eq!(list["removals"].as_array().unwrap().len(), 2);
        app.finish().await;
    }
}
