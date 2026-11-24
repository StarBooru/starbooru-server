//! `GET /home`: the home page an app shows at start, when an admin has
//! turned it on. Its background is a search (by default the most popular
//! posts rated safe), worked out once every few minutes; each request then
//! only checks those posts by id, so binned, re-rated and blacklisted ones
//! drop out at once.

use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, TimeDelta, Utc};
use serde::Deserialize;
use sqlx::{FromRow, Postgres, QueryBuilder};
use starbooru_core::color::stored_hex;
use starbooru_core::home::background_query;
use starbooru_core::{Home, HomePost, HomeSettings, ScoreKind, SearchQuery};
use tokio::sync::Mutex;
use tracing::warn;

use super::blacklist::{Hidden, hidden_for};
use super::posts::{
    TagFilter, correct_typos, media_urls, push_filters, push_order, sample_url, tag_filter,
};
use crate::error::AppResult;
use crate::guests::Viewer;
use crate::settings;
use crate::state::AppState;

/// How long a background is kept before the search runs again.
const HOME_TTL: TimeDelta = TimeDelta::minutes(5);
/// Posts kept per post shown, so a blacklist still leaves enough.
const SPARE: u32 = 2;

/// The background's search, last run.
#[derive(Default)]
pub struct HomeCache(Mutex<Option<Cached>>);

struct Cached {
    /// What it was worked out for.
    key: Key,
    at: DateTime<Utc>,
    query: SearchQuery,
    /// `None`: a tag the search needs doesn't exist.
    filter: Option<TagFilter>,
    /// The posts it found, in order.
    ids: Vec<i64>,
}

#[derive(Clone, PartialEq, Eq)]
struct Key {
    query: String,
    posts: u32,
    kind: ScoreKind,
}

impl HomeCache {
    /// Makes the next request run the search again.
    #[cfg(test)]
    pub async fn expire(&self) {
        if let Some(cached) = self.0.lock().await.as_mut() {
            cached.at = DateTime::<Utc>::MIN_UTC;
        }
    }
}

#[derive(Deserialize)]
pub struct HomeParams {
    /// A guest's own blacklist.
    exclude: Option<String>,
}

#[derive(FromRow)]
struct HomeRow {
    id: i64,
    sha256: Vec<u8>,
    ext: String,
    has_sample: Option<bool>,
    width: i32,
    height: i32,
    palette: Vec<i32>,
}

/// `GET /home`. Off, or before signing in to a private instance, it has no
/// posts: a private collection shows nothing to someone not signed in.
pub async fn home(
    State(state): State<AppState>,
    viewer: Option<Viewer>,
    Query(params): Query<HomeParams>,
) -> AppResult<Json<Home>> {
    let settings = settings::current(&state).await?;
    let home = settings.home.unwrap_or_default();
    let mut page = Home {
        enabled: home.enabled,
        title: home.title.clone().unwrap_or(settings.name),
        subtitle: home.subtitle.clone(),
        posts: Vec::new(),
    };
    let Some(viewer) = viewer.filter(|_| home.enabled) else {
        return Ok(Json(page));
    };
    let hidden = hidden_for(&state, &viewer, params.exclude.as_deref()).await?;
    page.posts = background(&state, &home, settings.score_kind, &hidden).await?;
    Ok(Json(page))
}

/// The background's posts for a reader: the cached search's, checked again,
/// and without what `hidden` lists.
async fn background(
    state: &AppState,
    home: &HomeSettings,
    kind: ScoreKind,
    hidden: &Hidden,
) -> AppResult<Vec<HomePost>> {
    let key = Key {
        query: home.query.clone(),
        posts: home.posts,
        kind,
    };
    let (query, filter, ids) = {
        let mut cache = state.home.0.lock().await;
        let fresh = cache
            .as_ref()
            .is_some_and(|c| c.key == key && Utc::now() - c.at < HOME_TTL);
        if !fresh {
            *cache = Some(search(state, key).await?);
        }
        match cache.as_ref() {
            Some(c) => (c.query.clone(), c.filter.clone(), c.ids.clone()),
            None => return Ok(Vec::new()),
        }
    };
    let Some(mut filter) = filter else {
        return Ok(Vec::new());
    };
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    // The admin's search doesn't outweigh the reader's list.
    filter.hide_all(hidden);
    let mut qb = QueryBuilder::<Postgres>::new(
        "SELECT p.id, p.sha256, p.ext, p.has_sample, p.width, p.height, p.palette \
         FROM unnest(",
    );
    qb.push_bind(ids)
        .push("::bigint[]) WITH ORDINALITY AS page(id, n) JOIN posts p ON p.id = page.id");
    push_filters(&mut qb, &query, &filter);
    qb.push(" AND p.has_thumb AND p.width > 0 AND p.height > 0 ORDER BY page.n LIMIT ")
        .push_bind(i64::from(home.posts));
    let rows: Vec<HomeRow> = qb.build_query_as().fetch_all(&state.db).await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let sha = hex::encode(&row.sha256);
            let (_, thumb) = media_urls(state, &sha, &row.ext, true);
            HomePost {
                id: row.id,
                thumb_url: thumb.unwrap_or_default(),
                sample_url: sample_url(state, &sha, row.has_sample),
                width: u32::try_from(row.width).unwrap_or(0),
                height: u32::try_from(row.height).unwrap_or(0),
                palette: row.palette.iter().copied().map(stored_hex).collect(),
            }
        })
        .collect())
}

/// Runs the background's search: posts with a thumbnail and a size, in the
/// search's order, a few more than shown.
async fn search(state: &AppState, key: Key) -> AppResult<Cached> {
    let query = match background_query(&key.query) {
        Ok(query) => query,
        Err(e) => {
            // Checked when saved; a later version may read it otherwise.
            warn!("the home page's search no longer reads ({e}); showing the popular posts");
            background_query("")?
        }
    };
    let (query, _) = correct_typos(state, query).await?;
    let filter = tag_filter(state, &query, &Hidden::default()).await?;
    let mut ids = Vec::new();
    if let Some(filter) = &filter {
        let mut qb = QueryBuilder::<Postgres>::new("SELECT p.id FROM posts p");
        push_filters(&mut qb, &query, filter);
        qb.push(" AND p.has_thumb AND p.width > 0 AND p.height > 0");
        push_order(&mut qb, query.order, None, key.kind);
        qb.push(" LIMIT ")
            .push_bind(i64::from(key.posts.saturating_mul(SPARE)));
        ids = qb.build_query_scalar().fetch_all(&state.db).await?;
    }
    Ok(Cached {
        key,
        at: Utc::now(),
        query,
        filter,
        ids,
    })
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use serde_json::{Value, json};
    use starbooru_core::Mode;

    use crate::testing::{self, TestApp};

    fn ids(page: &Value) -> Vec<i64> {
        page["posts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_i64().unwrap())
            .collect()
    }

    /// Sets the home page up as an admin does, keeping the other settings.
    async fn set_home(app: &TestApp, admin: &str, home: Value) -> testing::Resp {
        let mut settings = app.get("/api/v1/admin/settings", admin).await.json();
        settings["home"] = home;
        app.call(
            Method::PUT,
            "/api/v1/admin/settings",
            Some(admin),
            Some(settings),
        )
        .await
    }

    async fn vote(app: &TestApp, token: &str, id: i64, value: i64) {
        let r = app
            .call(
                Method::PUT,
                &format!("/api/v1/posts/{id}/vote"),
                Some(token),
                Some(json!({ "value": value })),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_private_home_page_shows_posts_after_sign_in() {
        let Some(app) = testing::app().await else {
            return;
        };
        let admin = app.register("admin").await;
        let bob = app.register("bob").await;
        let one = app.upload_png(&admin, 1, "sky").await;
        let two = app.upload_png(&admin, 2, "sky").await;
        let three = app.upload_png(&admin, 3, "sea").await;
        let spicy = app.upload_png(&admin, 4, "sky").await;
        app.call(
            Method::PATCH,
            &format!("/api/v1/posts/{spicy}"),
            Some(&admin),
            Some(json!({ "rating": "questionable" })),
        )
        .await;
        for token in [&admin, &bob] {
            vote(&app, token, one, 1).await;
            vote(&app, token, spicy, 1).await;
        }
        vote(&app, &admin, two, 1).await;

        // Off by default, saying so before anyone signs in.
        let info = app
            .call(Method::GET, "/api/v1/info", None, None)
            .await
            .json();
        assert_eq!(info["home"], false);
        let page = app.call(Method::GET, "/api/v1/home", None, None).await;
        assert_eq!(page.status, StatusCode::OK);
        assert_eq!(
            page.json(),
            json!({ "enabled": false, "title": "StarBooru", "subtitle": null, "posts": [] })
        );
        assert_eq!(
            app.get("/api/v1/home", &bob).await.json()["posts"],
            json!([])
        );
        let settings = app.get("/api/v1/admin/settings", &admin).await.json();
        assert_eq!(
            settings["home"],
            json!({ "enabled": false, "title": null, "subtitle": null, "query": "", "posts": 36 })
        );

        let r = set_home(
            &app,
            &admin,
            json!({ "enabled": true, "title": " Welcome ", "subtitle": "Skies and seas", "posts": 12 }),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        assert_eq!(r.json()["home"]["title"], "Welcome");
        let info = app
            .call(Method::GET, "/api/v1/info", None, None)
            .await
            .json();
        assert_eq!(info["home"], true);
        // Before signing in: the words, not the pictures.
        let page = app
            .call(Method::GET, "/api/v1/home", None, None)
            .await
            .json();
        assert_eq!(
            (
                &page["enabled"],
                &page["title"],
                &page["subtitle"],
                &page["posts"]
            ),
            (
                &json!(true),
                &json!("Welcome"),
                &json!("Skies and seas"),
                &json!([])
            )
        );
        // Signed in: the most popular posts rated safe.
        let page = app.get("/api/v1/home", &bob).await.json();
        assert_eq!(ids(&page), [one, two, three]);
        let post = &page["posts"][0];
        assert!(
            post["thumb_url"]
                .as_str()
                .unwrap()
                .starts_with("/media/thumb/")
        );
        assert!(post["width"].as_u64().unwrap() > 0 && post["height"].as_u64().unwrap() > 0);
        assert!(post.get("tags").is_none());
        let thumb = app
            .call(Method::GET, post["thumb_url"].as_str().unwrap(), None, None)
            .await;
        assert_eq!(thumb.status, StatusCode::OK);
        // A bad token is still refused.
        assert_eq!(
            app.get("/api/v1/home", "sb_nope").await.status,
            StatusCode::UNAUTHORIZED
        );

        // Kept for a few minutes: new votes wait, a binned post goes at once.
        vote(&app, &admin, three, 1).await;
        vote(&app, &bob, three, 1).await;
        vote(&app, &bob, two, 1).await;
        app.call(
            Method::DELETE,
            &format!("/api/v1/posts/{one}"),
            Some(&admin),
            None,
        )
        .await;
        assert_eq!(
            ids(&app.get("/api/v1/home", &bob).await.json()),
            [two, three]
        );
        app.state.home.expire().await;
        assert_eq!(
            ids(&app.get("/api/v1/home", &bob).await.json()),
            [three, two]
        );

        // The reader's blacklist applies.
        app.call(
            Method::PUT,
            "/api/v1/auth/blacklist",
            Some(&bob),
            Some(json!({ "entries": ["sea"] })),
        )
        .await;
        assert_eq!(ids(&app.get("/api/v1/home", &bob).await.json()), [two]);
        assert_eq!(ids(&app.get("/api/v1/home", &admin).await.json()).len(), 2);

        // A search of the admin's own, at once: `rating:safe` unless it names
        // a rating.
        let r = set_home(
            &app,
            &admin,
            json!({ "enabled": true, "query": "sky", "posts": 12 }),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK);
        let page = app.get("/api/v1/home", &admin).await.json();
        assert_eq!(page["title"], "StarBooru");
        assert_eq!(ids(&page), [two]);
        // The admin's search names `sky`, but the reader didn't: their list wins.
        app.call(
            Method::PUT,
            "/api/v1/auth/blacklist",
            Some(&bob),
            Some(json!({ "entries": ["sky"] })),
        )
        .await;
        assert_eq!(
            app.get("/api/v1/home", &bob).await.json()["posts"],
            json!([])
        );
        set_home(
            &app,
            &admin,
            json!({ "enabled": true, "query": "sky rating:questionable", "posts": 12 }),
        )
        .await;
        assert_eq!(ids(&app.get("/api/v1/home", &admin).await.json()), [spicy]);
        set_home(
            &app,
            &admin,
            json!({ "enabled": true, "query": "nothing_here", "posts": 12 }),
        )
        .await;
        assert_eq!(
            app.get("/api/v1/home", &admin).await.json()["posts"],
            json!([])
        );

        // What an admin can't save.
        for bad in [
            json!({ "enabled": true, "query": "rating:zzz" }),
            json!({ "enabled": true, "query": "status:deleted" }),
            json!({ "enabled": true, "posts": 5 }),
            json!({ "enabled": true, "posts": 61 }),
            json!({ "enabled": true, "title": "t".repeat(65) }),
        ] {
            let r = set_home(&app, &admin, bad.clone()).await;
            assert_eq!(r.status, StatusCode::BAD_REQUEST, "{bad}");
        }
        // Settings from an app that doesn't know the home page leave it be.
        let mut old = app.get("/api/v1/admin/settings", &admin).await.json();
        old.as_object_mut().unwrap().remove("home");
        app.call(
            Method::PUT,
            "/api/v1/admin/settings",
            Some(&admin),
            Some(old),
        )
        .await;
        let settings = app.get("/api/v1/admin/settings", &admin).await.json();
        assert_eq!(settings["home"]["query"], "nothing_here");

        // Turned off, nobody gets posts.
        set_home(&app, &admin, json!({ "enabled": false })).await;
        let page = app.get("/api/v1/home", &admin).await.json();
        assert_eq!(
            (&page["enabled"], &page["posts"]),
            (&json!(false), &json!([]))
        );
        app.finish().await;
    }

    #[tokio::test]
    async fn guests_see_a_public_home_page() {
        let Some(app) = testing::app_with(|c| c.mode = Some(Mode::Public)).await else {
            return;
        };
        let admin = app.register("admin").await;
        let cat = app.upload_png(&admin, 1, "cat").await;
        let dog = app.upload_png(&admin, 2, "dog").await;
        vote(&app, &admin, cat, 1).await;
        let page = app
            .call(Method::GET, "/api/v1/home", None, None)
            .await
            .json();
        assert_eq!(
            (&page["enabled"], &page["posts"]),
            (&json!(false), &json!([]))
        );

        let r = set_home(&app, &admin, json!({ "enabled": true })).await;
        assert_eq!(r.status, StatusCode::OK);
        let guest = |path: &'static str| {
            let app = &app;
            async move {
                let r = app.call(Method::GET, path, None, None).await;
                assert_eq!(r.status, StatusCode::OK);
                ids(&r.json())
            }
        };
        assert_eq!(guest("/api/v1/home").await, [cat, dog]);
        assert_eq!(guest("/api/v1/home?exclude=cat").await, [dog]);
        // The admin's default applies to guests who send none.
        let mut settings = app.get("/api/v1/admin/settings", &admin).await.json();
        settings["default_blacklist"] = json!(["dog"]);
        app.call(
            Method::PUT,
            "/api/v1/admin/settings",
            Some(&admin),
            Some(settings),
        )
        .await;
        assert_eq!(guest("/api/v1/home").await, [cat]);
        assert_eq!(guest("/api/v1/home?exclude=").await, [cat, dog]);
        app.finish().await;
    }
}
