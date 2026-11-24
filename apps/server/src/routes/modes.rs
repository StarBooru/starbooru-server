//! Switching between private and public, and previews of what a switch or
//! a change of score kind would delete or convert. Admins only; the switch
//! needs a sign-in session, like backups.

use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use starbooru_core::{Mode, ModePreview, ModeSwitch, ModeSwitched, ScoreKind, ScorePreview};

use super::admin::require_admin_session;
use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::{modes, scores, settings};

#[derive(Deserialize)]
pub struct To {
    #[serde(default)]
    to: String,
}

/// `GET /admin/mode/preview?to=public|private`.
pub async fn preview(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<To>,
) -> AppResult<Json<ModePreview>> {
    user.require_admin()?;
    let to = Mode::parse(q.to.trim())
        .ok_or_else(|| AppError::BadRequest("`to` is private or public".into()))?;
    let changes = modes::preview(&state, to).await?;
    Ok(Json(ModePreview { to, changes }))
}

/// `POST /admin/mode`: answers once the switch is done.
pub async fn switch(
    State(state): State<AppState>,
    user: AuthUser,
    Json(req): Json<ModeSwitch>,
) -> AppResult<Json<ModeSwitched>> {
    require_admin_session(&user)?;
    modes::switch(&state, req.to, req.backup).await.map(Json)
}

/// `GET /admin/scores/preview?to=likes|stars`.
pub async fn scores_preview(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<To>,
) -> AppResult<Json<ScorePreview>> {
    user.require_admin()?;
    let to = ScoreKind::parse(q.to.trim())
        .ok_or_else(|| AppError::BadRequest("`to` is likes or stars".into()))?;
    let (mode, kind) = settings::scoring(&state.db).await?;
    check_kind(mode, to)?;
    let changes = if to == kind {
        Vec::new()
    } else {
        scores::plan(&state.db, to).await?
    };
    Ok(Json(ScorePreview { to, changes }))
}

/// Stars are for private instances.
pub(crate) fn check_kind(mode: Mode, kind: ScoreKind) -> AppResult<()> {
    if kind.in_mode(mode) == kind {
        Ok(())
    } else {
        Err(AppError::Conflict {
            code: "public_counts_likes",
            message: "a public server counts likes and dislikes; stars are for private ones".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use serde_json::{Value, json};

    use crate::testing::{self, TestApp};

    fn has_pg_dump() -> bool {
        std::process::Command::new("pg_dump")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    async fn put(app: &TestApp, token: &str, path: &str, body: Value) -> testing::Resp {
        app.call(Method::PUT, path, Some(token), Some(body)).await
    }

    async fn post(app: &TestApp, token: &str, path: &str, body: Value) -> testing::Resp {
        app.call(Method::POST, path, Some(token), Some(body)).await
    }

    fn kinds(changes: &Value) -> Vec<(String, u64)> {
        changes
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                assert!(!c["message"].as_str().unwrap().is_empty());
                (
                    c["kind"].as_str().unwrap().to_owned(),
                    c["count"].as_u64().unwrap(),
                )
            })
            .collect()
    }

    fn pairs(list: &[(&str, u64)]) -> Vec<(String, u64)> {
        list.iter().map(|(k, n)| ((*k).to_owned(), *n)).collect()
    }

    #[tokio::test]
    async fn going_public_previews_backs_up_and_cleans_up() {
        if !has_pg_dump() {
            eprintln!("skipped: pg_dump is not installed");
            return;
        }
        let Some(app) = testing::app_with(|c| c.pg_dump = Some("pg_dump".into())).await else {
            return;
        };
        let admin = app.register("admin").await;
        let user = app.register("someone").await;
        let a = app.upload_png(&admin, 1, "sky").await;
        let b = app.upload_png(&admin, 2, "sky").await;

        // What a private instance has: links, other boorus, star votes.
        let mut tokens = Vec::new();
        for ids in [vec![a], vec![a, b]] {
            let r = post(&app, &admin, "/api/v1/shares", json!({ "post_ids": ids })).await;
            assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
            tokens.push(r.json()["token"].as_str().unwrap().to_owned());
        }
        let r = put(
            &app,
            &user,
            "/api/v1/remote/accounts",
            json!({ "api": "danbooru", "base": "https://danbooru.donmai.us",
                    "login": "me", "api_key": "k3y" }),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        let r = post(
            &app,
            &user,
            "/api/v1/remote/searches",
            json!({ "name": "cats", "query": "cat",
                    "sites": [{ "api": "danbooru", "base": "https://danbooru.donmai.us" }] }),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        let r = put(
            &app,
            &admin,
            "/api/v1/admin/settings",
            json!({ "name": "Home", "allow_registration": true, "trash_days": 0,
                    "score_kind": "stars" }),
        )
        .await;
        assert_eq!(r.json()["score_kind"], "stars");
        for (token, stars) in [(&admin, 5), (&user, 3)] {
            let r = put(
                &app,
                token,
                &format!("/api/v1/posts/{a}/vote"),
                json!({ "stars": stars }),
            )
            .await;
            assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        }

        // The preview counts, and changes nothing.
        let preview = |to: &str| {
            let (app, admin, path) = (
                &app,
                admin.clone(),
                format!("/api/v1/admin/mode/preview?to={to}"),
            );
            async move { app.get(&path, &admin).await }
        };
        let r = preview("public").await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(r.json()["to"], "public");
        assert_eq!(
            kinds(&r.json()["changes"]),
            pairs(&[
                ("share_links", 2),
                ("booru_accounts", 1),
                ("remote_searches", 1),
                ("star_votes_converted", 1),
                ("star_votes_dropped", 1),
            ])
        );
        assert_eq!(
            r.json()["changes"][0]["message"],
            "2 share links will be deleted; on a public server a post is shared by its own address"
        );
        assert_eq!(preview("private").await.json()["changes"], json!([]));
        assert_eq!(preview("open").await.status, StatusCode::BAD_REQUEST);
        let r = app.get("/api/v1/admin/mode/preview?to=public", &user).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN);
        assert_eq!(kinds(&preview("public").await.json()["changes"]).len(), 5);

        // A session admin only; a key may look.
        let key = post(&app, &admin, "/api/v1/keys", json!({ "name": "script" }))
            .await
            .json()["token"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            app.get("/api/v1/admin/mode/preview?to=public", &key)
                .await
                .status,
            200
        );
        let r = post(&app, &key, "/api/v1/admin/mode", json!({ "to": "public" })).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN);
        let r = post(&app, &user, "/api/v1/admin/mode", json!({ "to": "public" })).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN);

        // The switch: a backup, then everything at once.
        let r = post(
            &app,
            &admin,
            "/api/v1/admin/mode",
            json!({ "to": "public" }),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        let done = r.json();
        assert_eq!(done["settings"]["mode"], "public");
        assert_eq!(done["settings"]["score_kind"], "likes");
        assert_eq!(done["settings"]["name"], "Home");
        assert_eq!(
            kinds(&done["changes"]),
            pairs(&[
                ("share_links", 2),
                ("booru_accounts", 1),
                ("remote_searches", 1),
                ("star_votes_converted", 1),
                ("star_votes_dropped", 1),
            ])
        );
        assert!(
            done["changes"][0]["message"]
                .as_str()
                .unwrap()
                .contains("were deleted")
        );
        let backup = done["backup"].as_str().unwrap();
        let file = app.state.config.backup_dir.join(backup);
        assert!(
            std::fs::read(&file).unwrap().starts_with(b"PGDMP"),
            "{file:?}"
        );
        for table in ["shares", "booru_accounts", "remote_searches"] {
            let n: i64 =
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                    .fetch_one(app.db())
                    .await
                    .unwrap();
            assert_eq!(n, 0, "{table}");
        }

        // /info tells apps before they sign in.
        let info = app
            .call(Method::GET, "/api/v1/info", None, None)
            .await
            .json();
        assert_eq!(
            (&info["mode"], &info["score_kind"], &info["guests"]),
            (&json!("public"), &json!("likes"), &json!(true))
        );

        // The 5 became a like, the 3 went.
        let p = app.get(&format!("/api/v1/posts/{a}"), &admin).await.json();
        assert_eq!(
            (p["score"].as_i64(), p["my_vote"].as_i64()),
            (Some(1), Some(1))
        );
        assert!(p.get("stars").is_none() && p.get("votes").is_none());
        let p = app.get(&format!("/api/v1/posts/{a}"), &user).await.json();
        assert_eq!(p["my_vote"], 0);
        let r = put(
            &app,
            &user,
            &format!("/api/v1/posts/{a}/vote"),
            json!({ "stars": 4 }),
        )
        .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);

        // Sharing goes by address, other boorus are off.
        let r = post(&app, &admin, "/api/v1/shares", json!({ "post_ids": [a] })).await;
        assert_eq!(
            (r.status, r.json()["code"].clone()),
            (StatusCode::CONFLICT, json!("share_by_address"))
        );
        assert_eq!(app.get("/api/v1/shares", &admin).await.json(), json!([]));
        let r = app
            .call(
                Method::GET,
                &format!("/api/v1/shared/{}", tokens[0]),
                None,
                None,
            )
            .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
        let remote = [
            (Method::POST, "/api/v1/remote/search"),
            (Method::POST, "/api/v1/remote/tags"),
            (Method::POST, "/api/v1/remote/post"),
            (Method::GET, "/api/v1/remote/media?u=x&exp=1&sig=00"),
            (Method::GET, "/api/v1/remote/searches"),
            (Method::POST, "/api/v1/remote/searches"),
            (Method::DELETE, "/api/v1/remote/searches/1"),
            (Method::GET, "/api/v1/remote/accounts"),
            (Method::PUT, "/api/v1/remote/accounts"),
            (Method::DELETE, "/api/v1/remote/accounts/1"),
        ];
        for (method, path) in &remote {
            let r = app
                .call(method.clone(), path, Some(&user), Some(json!({})))
                .await;
            assert_eq!(r.status, StatusCode::FORBIDDEN, "{method} {path}");
            assert_eq!(r.json()["code"], "remote_off", "{method} {path}");
        }
        // Libraries come in by an administrator only.
        let site = json!({ "source": "lanbooru", "base": "http://127.0.0.1:9", "unrated": "safe" });
        let r = post(&app, &user, "/api/v1/library-imports", site.clone()).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN);
        let r = post(&app, &admin, "/api/v1/library-imports", site).await;
        assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.json());

        // Stars stay private; the mode moves only by the switch.
        let settings = |extra: Value| {
            let mut body = json!({ "name": "Home", "allow_registration": true, "trash_days": 0 });
            body.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            body
        };
        let r = put(
            &app,
            &admin,
            "/api/v1/admin/settings",
            settings(json!({ "score_kind": "stars" })),
        )
        .await;
        assert_eq!(
            (r.status, r.json()["code"].clone()),
            (StatusCode::CONFLICT, json!("public_counts_likes"))
        );
        let r = app
            .get("/api/v1/admin/scores/preview?to=stars", &admin)
            .await;
        assert_eq!(r.status, StatusCode::CONFLICT);
        let r = put(
            &app,
            &admin,
            "/api/v1/admin/settings",
            settings(json!({ "mode": "private" })),
        )
        .await;
        assert_eq!(
            (r.status, r.json()["code"].clone()),
            (StatusCode::CONFLICT, json!("use_mode_switch"))
        );
        // What GET gave goes back as it is; an old app leaves both out.
        let r = put(
            &app,
            &admin,
            "/api/v1/admin/settings",
            settings(json!({ "mode": "public", "score_kind": "likes" })),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        let r = put(
            &app,
            &admin,
            "/api/v1/admin/settings",
            settings(json!({ "name": "Open" })),
        )
        .await;
        assert_eq!(
            (r.json()["name"].clone(), r.json()["mode"].clone()),
            (json!("Open"), json!("public"))
        );
        let r = post(
            &app,
            &admin,
            "/api/v1/admin/mode",
            json!({ "to": "public", "backup": false }),
        )
        .await;
        assert_eq!(
            (r.status, r.json()["code"].clone()),
            (StatusCode::CONFLICT, json!("same_mode"))
        );

        // And back: nothing to delete, likes stay likes.
        assert_eq!(preview("private").await.json()["changes"], json!([]));
        let r = post(
            &app,
            &admin,
            "/api/v1/admin/mode",
            json!({ "to": "private", "backup": false }),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        assert_eq!(r.json()["changes"], json!([]));
        assert_eq!(r.json()["backup"], Value::Null);
        assert_eq!(r.json()["settings"]["score_kind"], "likes");
        let r = post(&app, &admin, "/api/v1/shares", json!({ "post_ids": [a] })).await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(
            app.get("/api/v1/remote/searches", &user).await.status,
            StatusCode::OK
        );
        app.finish().await;
    }

    #[tokio::test]
    async fn a_switch_without_backups_has_to_say_so() {
        // Tests run without pg_dump.
        let Some(app) = testing::app().await else {
            return;
        };
        let admin = app.register("admin").await;
        let a = app.upload_png(&admin, 1, "").await;
        post(&app, &admin, "/api/v1/shares", json!({ "post_ids": [a] })).await;
        let r = post(
            &app,
            &admin,
            "/api/v1/admin/mode",
            json!({ "to": "public" }),
        )
        .await;
        assert_eq!(
            (r.status, r.json()["code"].clone()),
            (StatusCode::CONFLICT, json!("backup_unavailable"))
        );
        assert_eq!(
            app.get("/api/v1/shares", &admin)
                .await
                .json()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            crate::settings::mode(&app.state).await.unwrap(),
            starbooru_core::Mode::Private
        );
        let r = post(
            &app,
            &admin,
            "/api/v1/admin/mode",
            json!({ "to": "public", "backup": false }),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        assert_eq!(kinds(&r.json()["changes"]), pairs(&[("share_links", 1)]));
        let r = post(
            &app,
            &admin,
            "/api/v1/admin/mode",
            json!({ "to": "secret" }),
        )
        .await;
        assert!(r.status.is_client_error());
        app.finish().await;
    }

    #[tokio::test]
    async fn stars_average_sort_and_convert_both_ways() {
        let Some(app) = testing::app().await else {
            return;
        };
        let admin = app.register("admin").await;
        let users = [
            admin.clone(),
            app.register("ann").await,
            app.register("bea").await,
        ];
        let one_five = app.upload_png(&admin, 1, "sky").await;
        let three_fours = app.upload_png(&admin, 2, "sky").await;
        let unvoted = app.upload_png(&admin, 3, "sky").await;
        let one_one = app.upload_png(&admin, 4, "sky").await;
        let vote = |token: &str, id: i64, body: Value| {
            let (app, token, path) = (&app, token.to_owned(), format!("/api/v1/posts/{id}/vote"));
            async move { app.call(Method::PUT, &path, Some(&token), Some(body)).await }
        };

        // Likes first, as today: an old app's -1 and 1.
        let r = vote(&users[1], one_five, json!({ "value": 1 })).await;
        assert_eq!(
            (r.json()["score"].as_i64(), r.json()["my_vote"].as_i64()),
            (Some(1), Some(1))
        );
        vote(&users[2], one_one, json!({ "value": -1 })).await;
        assert_eq!(
            vote(&admin, one_five, json!({ "stars": 5 })).await.status,
            StatusCode::BAD_REQUEST
        );

        // Likes to stars: previewed, then converted in the settings' answer.
        let r = app
            .get("/api/v1/admin/scores/preview?to=stars", &admin)
            .await;
        assert_eq!(
            kinds(&r.json()["changes"]),
            pairs(&[("like_votes_converted", 2)])
        );
        let settings = json!({ "name": "S", "allow_registration": true, "trash_days": 0 });
        let mut stars = settings.clone();
        stars["score_kind"] = json!("stars");
        let r = put(&app, &admin, "/api/v1/admin/settings", stars.clone()).await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        assert_eq!(r.json()["score_kind"], "stars");
        assert_eq!(
            kinds(&r.json()["changes"]),
            pairs(&[("like_votes_converted", 2)])
        );
        let p = app
            .get(&format!("/api/v1/posts/{one_five}"), &users[1])
            .await
            .json();
        assert_eq!(
            (
                p["stars"].as_f64(),
                p["votes"].as_u64(),
                p["my_stars"].as_u64()
            ),
            (Some(5.0), Some(1), Some(5))
        );
        let p = app
            .get(&format!("/api/v1/posts/{one_one}"), &users[2])
            .await
            .json();
        assert_eq!(
            (
                p["stars"].as_f64(),
                p["my_stars"].as_u64(),
                p["score"].as_i64()
            ),
            (Some(1.0), Some(1), Some(-1))
        );
        let info = app
            .call(Method::GET, "/api/v1/info", None, None)
            .await
            .json();
        assert_eq!(info["score_kind"], "stars");
        // Saving again (an old app, without score_kind) converts nothing.
        let r = put(&app, &admin, "/api/v1/admin/settings", settings.clone()).await;
        assert_eq!(
            (
                r.json()["score_kind"].clone(),
                r.json().get("changes").is_none()
            ),
            (json!("stars"), true)
        );

        // Stars: 1 to 5, 0 removes; an old app's like is 5, its dislike 1.
        for token in &users {
            let r = vote(token, three_fours, json!({ "stars": 4 })).await;
            assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        }
        let r = vote(&users[2], one_five, json!({ "stars": 3 })).await;
        assert_eq!(
            (r.json()["stars"].as_f64(), r.json()["votes"].as_u64()),
            (Some(4.0), Some(2))
        );
        assert_eq!(
            (r.json()["my_vote"].as_i64(), r.json()["my_stars"].as_u64()),
            (Some(0), Some(3))
        );
        assert_eq!(r.json()["score"], 1, "a 3 counts as neither");
        let r = vote(&users[2], one_five, json!({ "stars": 0 })).await;
        assert_eq!(r.json()["votes"], 1);
        assert!(r.json().get("my_stars").is_none());
        for bad in [
            json!({ "stars": 6 }),
            json!({ "value": 2 }),
            json!({ "stars": -1 }),
        ] {
            let r = vote(&admin, unvoted, bad.clone()).await;
            assert!(r.status.is_client_error(), "{bad}");
        }
        let r = vote(&admin, unvoted, json!({ "value": 1 })).await;
        assert_eq!(
            (r.json()["my_stars"].as_u64(), r.json()["my_vote"].as_i64()),
            (Some(5), Some(1))
        );
        let r = vote(&admin, unvoted, json!({ "value": -1 })).await;
        assert_eq!(r.json()["my_stars"], 1);
        let r = vote(&admin, unvoted, json!({ "value": 0 })).await;
        assert!(r.json().get("stars").is_none());
        let t = app
            .get(&format!("/api/v1/posts/{three_fours}"), &admin)
            .await
            .json();
        assert_eq!(
            (
                t["stars"].as_f64(),
                t["votes"].as_u64(),
                t["score"].as_i64()
            ),
            (Some(4.0), Some(3), Some(3))
        );

        // order:score is the fair rating: three 4s beat one 5, no votes sit
        // in the middle, one 1 below.
        let order = |q: &'static str| {
            let (app, admin) = (&app, admin.clone());
            async move {
                app.get(&format!("/api/v1/posts?q={q}"), &admin)
                    .await
                    .json()["posts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| p["id"].as_i64().unwrap())
                    .collect::<Vec<_>>()
            }
        };
        assert_eq!(
            order("order:score").await,
            [three_fours, one_five, unvoted, one_one]
        );
        assert_eq!(
            order("order:score_asc").await,
            [one_one, unvoted, one_five, three_fours]
        );
        let n = app
            .get(
                &format!("/api/v1/posts/{one_five}/neighbors?q=order:score"),
                &admin,
            )
            .await
            .json();
        assert_eq!(
            (n["prev"].as_i64(), n["next"].as_i64()),
            (Some(three_fours), Some(unvoted))
        );

        // A removed account takes its stars with it.
        let ann = app.get("/api/v1/admin/users", &admin).await.json()[1]["id"]
            .as_i64()
            .unwrap();
        app.call(
            Method::DELETE,
            &format!("/api/v1/admin/users/{ann}"),
            Some(&admin),
            None,
        )
        .await;
        let t = app
            .get(&format!("/api/v1/posts/{three_fours}"), &admin)
            .await
            .json();
        assert_eq!(
            (t["votes"].as_u64(), t["score"].as_i64()),
            (Some(2), Some(2))
        );
        let o = app
            .get(&format!("/api/v1/posts/{one_five}"), &admin)
            .await
            .json();
        assert!(o.get("stars").is_none(), "ann's 5 was the only vote");

        // Stars to likes: 4 and 5 like, 1 and 2 dislike, 3 dropped.
        vote(&users[2], one_one, json!({ "stars": 2 })).await;
        vote(&admin, unvoted, json!({ "stars": 3 })).await;
        let r = app
            .get("/api/v1/admin/scores/preview?to=likes", &admin)
            .await;
        assert_eq!(
            kinds(&r.json()["changes"]),
            pairs(&[("star_votes_converted", 3), ("star_votes_dropped", 1)])
        );
        let mut likes = settings.clone();
        likes["score_kind"] = json!("likes");
        let r = put(&app, &admin, "/api/v1/admin/settings", likes).await;
        assert_eq!(
            kinds(&r.json()["changes"]),
            pairs(&[("star_votes_converted", 3), ("star_votes_dropped", 1)])
        );
        let t = app
            .get(&format!("/api/v1/posts/{three_fours}"), &admin)
            .await
            .json();
        assert_eq!(
            (t["score"].as_i64(), t["my_vote"].as_i64()),
            (Some(2), Some(1))
        );
        assert!(t.get("stars").is_none() && t.get("my_stars").is_none());
        let u = app
            .get(&format!("/api/v1/posts/{unvoted}"), &admin)
            .await
            .json();
        assert_eq!(
            (u["score"].as_i64(), u["my_vote"].as_i64()),
            (Some(0), Some(0))
        );
        let o = app
            .get(&format!("/api/v1/posts/{one_one}"), &users[2])
            .await
            .json();
        assert_eq!(o["my_vote"], -1);
        assert_eq!(
            order("order:score").await,
            [three_fours, unvoted, one_five, one_one]
        );
        let r = app
            .get("/api/v1/admin/scores/preview?to=likes", &admin)
            .await;
        assert_eq!(r.json()["changes"], json!([]));

        // And the way back gives the same likes.
        let r = put(&app, &admin, "/api/v1/admin/settings", stars).await;
        assert_eq!(
            kinds(&r.json()["changes"]),
            pairs(&[("like_votes_converted", 3)])
        );
        let t = app
            .get(&format!("/api/v1/posts/{three_fours}"), &admin)
            .await
            .json();
        assert_eq!(
            (
                t["stars"].as_f64(),
                t["votes"].as_u64(),
                t["score"].as_i64()
            ),
            (Some(5.0), Some(2), Some(2))
        );
        app.finish().await;
    }

    /// The mode and score kind as stored, as `(mode, score_kind)`.
    async fn stored(app: &TestApp) -> (Option<String>, Option<String>) {
        sqlx::query_as("SELECT mode, score_kind FROM server_settings")
            .fetch_one(app.db())
            .await
            .unwrap()
    }

    /// As a database upgraded from before modes: both unset.
    async fn unsettle(app: &TestApp) {
        sqlx::query("UPDATE server_settings SET mode = NULL, score_kind = NULL")
            .execute(app.db())
            .await
            .unwrap();
    }

    fn some(mode: &str, kind: &str) -> (Option<String>, Option<String>) {
        (Some(mode.to_owned()), Some(kind.to_owned()))
    }

    #[tokio::test]
    async fn an_upgraded_server_keeps_its_votes_whatever_the_environment_says() {
        use crate::modes::settle;
        use starbooru_core::{Mode, ScoreKind};
        let Some(app) = testing::app().await else {
            return;
        };
        let admin = app.register("admin").await;
        let a = app.upload_png(&admin, 1, "").await;
        put(
            &app,
            &admin,
            &format!("/api/v1/posts/{a}/vote"),
            json!({ "value": 1 }),
        )
        .await;
        unsettle(&app).await;

        // Stars would convert the like: likes stay, with a warning.
        let s = settle(app.db(), None, Some(ScoreKind::Stars))
            .await
            .unwrap();
        assert_eq!(
            (s.mode, s.kind, s.first),
            (Mode::Private, ScoreKind::Likes, true)
        );
        assert_eq!(s.warnings.len(), 1, "{:?}", s.warnings);
        let w = &s.warnings[0];
        assert!(
            w.starts_with("STARBOORU_SCORES=stars was not applied"),
            "{w}"
        );
        assert!(
            w.contains("1 like or dislike will become stars") && w.contains("Administration"),
            "{w}"
        );
        assert_eq!(stored(&app).await, some("private", "likes"));
        let p = app.get(&format!("/api/v1/posts/{a}"), &admin).await.json();
        assert_eq!(
            (p["score"].as_i64(), p["my_vote"].as_i64()),
            (Some(1), Some(1))
        );
        assert!(p.get("stars").is_none());
        let info = app
            .call(Method::GET, "/api/v1/info", None, None)
            .await
            .json();
        assert_eq!(
            (&info["mode"], &info["score_kind"]),
            (&json!("private"), &json!("likes"))
        );

        // From then on the variable changes nothing, with one warning.
        let s = settle(app.db(), None, Some(ScoreKind::Stars))
            .await
            .unwrap();
        assert_eq!((s.kind, s.first), (ScoreKind::Likes, false));
        assert_eq!(s.warnings.len(), 1);
        assert!(
            s.warnings[0].starts_with("STARBOORU_SCORES=stars has no effect"),
            "{:?}",
            s.warnings
        );
        assert!(s.warnings[0].contains("stored score kind, likes"));
        assert_eq!(stored(&app).await, some("private", "likes"));
        let s = settle(app.db(), None, None).await.unwrap();
        assert!(s.warnings.is_empty());

        // Going public would delete a share link: it stays private.
        post(&app, &admin, "/api/v1/shares", json!({ "post_ids": [a] })).await;
        unsettle(&app).await;
        let s = settle(app.db(), Some(Mode::Public), None).await.unwrap();
        assert_eq!((s.mode, s.kind), (Mode::Private, ScoreKind::Likes));
        assert!(
            s.warnings[0].starts_with("STARBOORU_MODE=public was not applied"),
            "{:?}",
            s.warnings
        );
        assert!(s.warnings[0].contains("1 share link will be deleted"));
        assert_eq!(s.warnings.len(), 1);
        assert_eq!(
            app.get("/api/v1/shares", &admin)
                .await
                .json()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(stored(&app).await, some("private", "likes"));

        // With nothing to delete or convert, the environment is followed:
        // public, and its likes are already what it counts.
        sqlx::query("DELETE FROM shares")
            .execute(app.db())
            .await
            .unwrap();
        unsettle(&app).await;
        let s = settle(app.db(), Some(Mode::Public), None).await.unwrap();
        assert_eq!(
            (s.mode, s.kind, s.warnings.is_empty()),
            (Mode::Public, ScoreKind::Likes, true)
        );
        assert_eq!(stored(&app).await, some("public", "likes"));
        let p = app.get(&format!("/api/v1/posts/{a}"), &admin).await.json();
        assert_eq!(p["score"], 1);
        app.finish().await;
    }

    #[tokio::test]
    async fn a_new_server_starts_as_the_environment_says_then_keeps_it() {
        use crate::modes::settle;
        use starbooru_core::{Mode, ScoreKind};
        let Some(app) = testing::app_with(|c| c.mode = Some(Mode::Public)).await else {
            return;
        };
        assert_eq!(stored(&app).await, some("public", "likes"));
        let info = app
            .call(Method::GET, "/api/v1/info", None, None)
            .await
            .json();
        assert_eq!(
            (&info["mode"], &info["guests"]),
            (&json!("public"), &json!(true))
        );
        assert_eq!(
            app.call(Method::GET, "/api/v1/posts", None, None)
                .await
                .status,
            StatusCode::OK
        );
        let forum = app.call(Method::GET, "/api/v1/forum", None, None).await;
        assert_eq!(forum.json().as_array().map(Vec::len), Some(3));

        // Changing the variables afterwards changes nothing.
        let s = settle(app.db(), Some(Mode::Private), Some(ScoreKind::Stars))
            .await
            .unwrap();
        assert_eq!(
            (s.mode, s.kind, s.first),
            (Mode::Public, ScoreKind::Likes, false)
        );
        assert_eq!(
            s.warnings.len(),
            1,
            "one warning for both: {:?}",
            s.warnings
        );
        assert!(s.warnings[0].contains("STARBOORU_MODE=private has no effect"));
        assert!(s.warnings[0].contains("STARBOORU_SCORES=stars has no effect"));
        assert_eq!(stored(&app).await, some("public", "likes"));
        let info = app
            .call(Method::GET, "/api/v1/info", None, None)
            .await
            .json();
        assert_eq!(info["mode"], "public");
        app.finish().await;

        // A new private one with stars has no votes to convert, so it gets stars.
        let Some(app) = testing::app_with(|c| c.score_kind = Some(ScoreKind::Stars)).await else {
            return;
        };
        assert_eq!(stored(&app).await, some("private", "stars"));
        let s = settle(app.db(), None, Some(ScoreKind::Likes))
            .await
            .unwrap();
        assert_eq!(s.kind, ScoreKind::Stars);
        assert!(
            s.warnings[0].contains("stored score kind, stars"),
            "{:?}",
            s.warnings
        );
        // An admin's change is stored as is, and the variable doesn't undo it.
        let admin = app.register("admin").await;
        let r = put(
            &app,
            &admin,
            "/api/v1/admin/settings",
            json!({ "name": "S", "allow_registration": true, "trash_days": 0, "score_kind": "likes" }),
        )
        .await;
        assert_eq!(r.json()["score_kind"], "likes");
        let s = settle(app.db(), None, Some(ScoreKind::Stars))
            .await
            .unwrap();
        assert_eq!(s.kind, ScoreKind::Likes);
        assert_eq!(stored(&app).await, some("private", "likes"));
        app.finish().await;
    }

    #[tokio::test]
    async fn going_private_deletes_what_only_a_public_server_shows() {
        let Some(app) = testing::app().await else {
            return;
        };
        let admin = app.register("admin").await;
        let ann = app.register("ann").await;
        let p = app.upload_png(&admin, 1, "sky cat").await;
        app.switch(&admin, "public").await;

        let png = {
            let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(32, 32));
            let mut out = std::io::Cursor::new(Vec::new());
            img.write_to(&mut out, image::ImageFormat::Png).unwrap();
            out.into_inner()
        };
        app.put_bytes("/api/v1/auth/avatar", Some(&ann), png).await;
        app.call(
            Method::PATCH,
            "/api/v1/auth/profile",
            Some(&ann),
            Some(json!({ "display_name": "Ann", "about": "Hello" })),
        )
        .await;
        let general = app.get("/api/v1/forum", &ann).await.json()[0]["id"].clone();
        let r = post(
            &app,
            &ann,
            "/api/v1/forum/threads",
            json!({ "category_id": general, "title": "Hi", "body": "First" }),
        )
        .await;
        let t = r.json()["id"].as_i64().unwrap();
        post(
            &app,
            &admin,
            &format!("/api/v1/forum/threads/{t}/posts"),
            json!({ "body": "Welcome" }),
        )
        .await;
        let mut s = app.get("/api/v1/admin/settings", &admin).await.json();
        s["tag_vote_threshold"] = json!(1);
        put(&app, &admin, "/api/v1/admin/settings", s).await;
        for (tag, value) in [("sky", 1), ("cat", -1)] {
            app.call(
                Method::PUT,
                &format!("/api/v1/posts/{p}/tags/{tag}/vote"),
                Some(&ann),
                Some(json!({ "value": value })),
            )
            .await;
        }

        let all = pairs(&[
            ("profile_pictures", 1),
            ("profile_texts", 1),
            ("forum_posts", 2),
            ("forum_threads", 1),
            ("tag_votes", 1),
            ("tag_removals", 1),
        ]);
        let r = app
            .get("/api/v1/admin/mode/preview?to=private", &admin)
            .await;
        assert_eq!(kinds(&r.json()["changes"]), all);
        let messages: Vec<String> = r.json()["changes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["message"].as_str().unwrap().to_owned())
            .collect();
        assert!(
            messages.contains(&"1 forum thread will be deleted; the categories stay".to_owned())
        );
        let r = app.switch(&admin, "private").await;
        assert_eq!(kinds(&r["changes"]), all);

        let me = app.get("/api/v1/auth/profile", &ann).await.json();
        assert_eq!(
            (&me["display_name"], &me["about"], &me["avatar_url"]),
            (&json!("Ann"), &Value::Null, &Value::Null)
        );
        assert!(app.state.storage.avatar_names().unwrap().is_empty());
        let post_now = app.get(&format!("/api/v1/posts/{p}"), &admin).await.json();
        assert_eq!(
            post_now["tags"],
            json!(["sky"]),
            "the voted-off tag stays off"
        );
        assert!(post_now.get("tag_votes").is_none() && post_now.get("voted_off").is_none());
        assert!(post_now.get("uploader").is_none());
        let r = app.get("/api/v1/admin/settings", &admin).await.json();
        assert!(r.get("tag_vote_threshold").is_none());
        // Edits may add it again.
        let r = app
            .call(
                Method::PATCH,
                &format!("/api/v1/posts/{p}"),
                Some(&admin),
                Some(json!({ "tags": ["sky", "cat"] })),
            )
            .await;
        assert_eq!(r.json()["tags"], json!(["cat", "sky"]));

        // Back to public: the categories are there, not twice; nothing to
        // delete for a private server.
        let r = app
            .get("/api/v1/admin/mode/preview?to=private", &admin)
            .await;
        assert_eq!(r.json()["changes"], json!([]));
        app.switch(&admin, "public").await;
        let cats = app.get("/api/v1/forum", &ann).await.json();
        assert_eq!(cats.as_array().unwrap().len(), 3);
        assert_eq!(cats[0]["threads"], 0);
        let r = app.get("/api/v1/admin/settings", &admin).await.json();
        assert_eq!(r["tag_vote_threshold"], 1, "the threshold was kept");
        app.finish().await;
    }

    #[tokio::test]
    async fn a_public_instance_opens_no_saved_booru_account() {
        let Some(app) = testing::app().await else {
            return;
        };
        let user = app.register("admin").await;
        put(
            &app,
            &user,
            "/api/v1/remote/accounts",
            json!({ "api": "danbooru", "base": "https://danbooru.donmai.us",
                    "login": "me", "api_key": "k3y" }),
        )
        .await;
        let id = app.get("/api/v1/auth/me", &user).await.json()["id"]
            .as_i64()
            .unwrap();
        let accounts = crate::remote::accounts::load(&app.state, id).await.unwrap();
        assert!(accounts.of("https://danbooru.donmai.us").is_some());
        crate::settings::save_mode(app.db(), starbooru_core::Mode::Public)
            .await
            .unwrap();
        let accounts = crate::remote::accounts::load(&app.state, id).await.unwrap();
        assert!(accounts.of("https://danbooru.donmai.us").is_none());
        app.finish().await;
    }
}
