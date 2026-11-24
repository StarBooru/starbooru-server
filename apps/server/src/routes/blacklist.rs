//! Tag blacklists (`api.md` › Blacklist): each account's tags and ratings
//! it doesn't want to see, the admin's default for new accounts and guests,
//! and what a guest sends instead. Reads leave those posts out through the
//! search's own exclusion (`TagFilter::hide`), unless the search names them.

use std::collections::{HashMap, HashSet};

use axum::Json;
use axum::extract::State;
use sqlx::PgExecutor;
use starbooru_core::blacklist::{
    Entry, MAX_BLACKLIST, clean_blacklist, parse_exclude, split_entries,
};
use starbooru_core::{Blacklist, Rating};

use crate::auth::AuthUser;
use crate::error::AppResult;
use crate::guests::Viewer;
use crate::settings;
use crate::state::AppState;

/// `GET /auth/blacklist`: the caller's list. A guest gets the admin's
/// default, which reads use until the guest sends their own.
pub async fn get(State(state): State<AppState>, viewer: Viewer) -> AppResult<Json<Blacklist>> {
    let entries = match &viewer {
        Viewer::User(user) => stored(&state.db, user.id).await?,
        Viewer::Guest => settings::default_blacklist(&state.db).await?,
    };
    Ok(Json(Blacklist { entries }))
}

/// `PUT /auth/blacklist`: replaces the caller's list.
pub async fn put(
    State(state): State<AppState>,
    user: AuthUser,
    Json(list): Json<Blacklist>,
) -> AppResult<Json<Blacklist>> {
    let entries: Vec<String> = clean_blacklist(&list.entries, MAX_BLACKLIST)?
        .iter()
        .map(Entry::as_entry)
        .collect();
    let entries = resolve_aliases(&state.db, &entries).await?;
    sqlx::query("UPDATE users SET blacklist = $2 WHERE id = $1")
        .bind(user.id)
        .bind(&entries)
        .execute(&state.db)
        .await?;
    Ok(Json(Blacklist { entries }))
}

async fn stored(db: impl PgExecutor<'_>, user_id: i64) -> AppResult<Vec<String>> {
    Ok(
        sqlx::query_scalar("SELECT blacklist FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(db)
            .await?
            .unwrap_or_default(),
    )
}

/// A cleaned list with each alias replaced by its tag, as tagging does, and
/// the repeats that makes dropped.
pub(super) async fn resolve_aliases(
    db: impl PgExecutor<'_>,
    entries: &[String],
) -> AppResult<Vec<String>> {
    let (tags, _) = split_entries(entries);
    if tags.is_empty() {
        return Ok(entries.to_vec());
    }
    let canonical: HashMap<String, String> = sqlx::query_as(
        "SELECT a.alias, t.name FROM tag_aliases a JOIN tags t ON t.id = a.tag_id \
         WHERE a.alias = ANY($1)",
    )
    .bind(&tags)
    .fetch_all(db)
    .await?
    .into_iter()
    .collect();
    let mut seen = HashSet::new();
    Ok(entries
        .iter()
        .map(|e| canonical.get(e).cloned().unwrap_or_else(|| e.clone()))
        .filter(|e| seen.insert(e.clone()))
        .collect())
}

/// A reader's blacklist, resolved: what reads leave out.
#[derive(Clone, Debug, Default)]
pub(super) struct Hidden {
    /// Each tag entry with the tags it hides: its own, through an alias,
    /// and every tag under it in the tree.
    tags: Vec<(String, Vec<i64>)>,
    ratings: Vec<Rating>,
}

impl Hidden {
    pub(super) fn is_empty(&self) -> bool {
        self.tags.is_empty() && self.ratings.is_empty()
    }

    /// The tags to leave out of a search that asks for `asked` itself:
    /// what the reader typed wins over the list.
    pub(super) fn tags_except(&self, asked: &HashSet<i64>) -> Vec<i64> {
        let mut ids: Vec<i64> = self
            .tags
            .iter()
            .flat_map(|(_, tree)| tree.iter().copied())
            .filter(|id| !asked.contains(id))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// The ratings to leave out of a search that asks for `asked`, as
    /// stored.
    pub(super) fn ratings_except(&self, asked: &[Rating]) -> Vec<i16> {
        self.ratings
            .iter()
            .filter(|r| !asked.contains(r))
            .map(|r| r.to_db())
            .collect()
    }

    /// The entries a post with `tag_ids` and `rating` matches.
    pub(super) fn matched(&self, tag_ids: &[i64], rating: Rating) -> Vec<String> {
        let mut out: Vec<String> = self
            .tags
            .iter()
            .filter(|(_, tree)| tree.iter().any(|id| tag_ids.contains(id)))
            .map(|(entry, _)| entry.clone())
            .collect();
        if self.ratings.contains(&rating) {
            out.push(Entry::Rating(rating).as_entry());
        }
        out
    }
}

/// What `viewer` doesn't want to see: an account's own list; for a guest,
/// the list they sent in `exclude`, or else the admin's default. An account
/// ignores `exclude`.
pub(super) async fn hidden_for(
    state: &AppState,
    viewer: &Viewer,
    exclude: Option<&str>,
) -> AppResult<Hidden> {
    let entries = match viewer {
        Viewer::User(user) => stored(&state.db, user.id).await?,
        Viewer::Guest => match exclude {
            Some(list) => parse_exclude(list)?.iter().map(Entry::as_entry).collect(),
            None => settings::default_blacklist(&state.db).await?,
        },
    };
    resolve(&state.db, &entries).await
}

/// Resolves a list: each tag entry, through an alias if it is one, to its
/// tag and the tags under it. Tags that don't exist hide nothing.
async fn resolve(db: impl PgExecutor<'_>, entries: &[String]) -> AppResult<Hidden> {
    let (names, ratings) = split_entries(entries);
    if names.is_empty() {
        return Ok(Hidden {
            tags: Vec::new(),
            ratings,
        });
    }
    let rows: Vec<(String, Vec<i64>)> = sqlx::query_as(
        "WITH RECURSIVE tree(entry, id, depth) AS ( \
             SELECT n.entry, COALESCE(a.tag_id, t.id), 0 FROM unnest($1::text[]) AS n(entry) \
             LEFT JOIN tag_aliases a ON a.alias = n.entry \
             LEFT JOIN tags t ON t.name = n.entry \
             WHERE COALESCE(a.tag_id, t.id) IS NOT NULL \
             UNION ALL SELECT tree.entry, t.id, tree.depth + 1 FROM tags t \
             JOIN tree ON t.parent_id = tree.id WHERE tree.depth < $2 \
         ) SELECT entry, array_agg(DISTINCT id) FROM tree GROUP BY entry",
    )
    .bind(&names)
    .bind(super::tags::MAX_DEPTH)
    .fetch_all(db)
    .await?;
    let mut found: HashMap<String, Vec<i64>> = rows.into_iter().collect();
    let tags = names
        .into_iter()
        .filter_map(|name| {
            let tree = found.remove(&name)?;
            Some((name, tree))
        })
        .collect();
    Ok(Hidden { tags, ratings })
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use serde_json::{Value, json};
    use starbooru_core::Mode;
    use starbooru_core::blacklist::MAX_EXCLUDE;

    use super::*;
    use crate::testing::{self, TestApp};

    #[test]
    fn what_a_search_names_stays_in() {
        let hidden = Hidden {
            tags: vec![("footwear".into(), vec![3, 1, 2]), ("cat".into(), vec![9])],
            ratings: vec![Rating::Explicit],
        };
        assert_eq!(hidden.tags_except(&HashSet::new()), [1, 2, 3, 9]);
        assert_eq!(hidden.tags_except(&HashSet::from([2, 9])), [1, 3]);
        assert_eq!(hidden.ratings_except(&[]), [Rating::Explicit.to_db()]);
        assert!(hidden.ratings_except(&[Rating::Explicit]).is_empty());
        assert_eq!(hidden.matched(&[2, 5], Rating::Safe), ["footwear"]);
        assert_eq!(
            hidden.matched(&[9, 1], Rating::Explicit),
            ["footwear", "cat", "rating:explicit"]
        );
        assert!(hidden.matched(&[5], Rating::Safe).is_empty());
        assert!(Hidden::default().is_empty());
    }

    fn ids(v: &Value) -> Vec<i64> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_i64().unwrap())
            .collect()
    }

    async fn put_json(app: &TestApp, token: &str, path: &str, body: Value) -> Value {
        let r = app
            .call(Method::PUT, path, Some(token), Some(body.clone()))
            .await;
        assert_eq!(r.status, StatusCode::OK, "{path} {body}: {:?}", r.json());
        r.json()
    }

    async fn set_blacklist(app: &TestApp, token: &str, entries: &[&str]) -> Value {
        put_json(
            app,
            token,
            "/api/v1/auth/blacklist",
            json!({ "entries": entries }),
        )
        .await
    }

    #[tokio::test]
    async fn an_account_blacklist_is_cleaned_and_kept() {
        let Some(app) = testing::app().await else {
            return;
        };
        let admin = app.register("admin").await;
        app.upload_png(&admin, 1, "cat").await;
        app.call(
            Method::POST,
            "/api/v1/tags/cat/aliases",
            Some(&admin),
            Some(json!({ "alias": "kitty" })),
        )
        .await;
        let empty = app.get("/api/v1/auth/blacklist", &admin).await;
        assert_eq!(empty.json(), json!({ "entries": [] }));

        let saved = set_blacklist(
            &app,
            &admin,
            &[
                "Kitty",
                "cat",
                "Rating:E",
                "  ",
                "artist:Some One",
                "not_yet",
            ],
        )
        .await;
        assert_eq!(
            saved["entries"],
            json!(["cat", "rating:explicit", "some_one", "not_yet"])
        );
        let got = app.get("/api/v1/auth/blacklist", &admin).await.json();
        assert_eq!(got, saved);

        for bad in [
            json!({ "entries": ["type:video"] }),
            json!({ "entries": ["-cat"] }),
            json!({ "entries": ["rating:x"] }),
            json!({ "entries": (0..=MAX_BLACKLIST).map(|i| format!("t{i}")).collect::<Vec<_>>() }),
        ] {
            let r = app
                .call(
                    Method::PUT,
                    "/api/v1/auth/blacklist",
                    Some(&admin),
                    Some(bad.clone()),
                )
                .await;
            assert_eq!(r.status, StatusCode::BAD_REQUEST, "{bad}");
            assert_eq!(r.json()["code"], "bad_request");
        }
        // Refused lists change nothing; an empty one clears it.
        assert_eq!(
            app.get("/api/v1/auth/blacklist", &admin).await.json(),
            saved
        );
        let cleared = set_blacklist(&app, &admin, &[]).await;
        assert_eq!(cleared["entries"], json!([]));
        let r = app
            .call(Method::GET, "/api/v1/auth/blacklist", None, None)
            .await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        app.finish().await;
    }

    /// Search, neighbours, related posts, Explore, a tag's posts and the
    /// colour search all leave blacklisted posts out, with the tag tree and
    /// aliases; a post opened by id opens, flagged.
    #[tokio::test]
    async fn reads_leave_blacklisted_posts_out() {
        let Some(app) = testing::app().await else {
            return;
        };
        let admin = app.register("admin").await;
        let a = app.upload_png(&admin, 1, "cat sky rare").await;
        let b = app.upload_png(&admin, 2, "boots sky").await;
        let c = app.upload_png(&admin, 3, "sky rare").await;
        let d = app.upload_png(&admin, 4, "sky").await;
        let e = app.upload_png(&admin, 5, "plain").await;
        let r = app
            .call(
                Method::PATCH,
                &format!("/api/v1/posts/{d}"),
                Some(&admin),
                Some(json!({ "rating": "explicit" })),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK);
        // `boots` under `footwear`, and `kitty` another name for `cat`.
        app.call(
            Method::PATCH,
            "/api/v1/tags/boots",
            Some(&admin),
            Some(json!({ "parent": "footwear" })),
        )
        .await;
        app.call(
            Method::POST,
            "/api/v1/tags/cat/aliases",
            Some(&admin),
            Some(json!({ "alias": "kitty" })),
        )
        .await;
        let search = |q: &str| {
            let path = format!("/api/v1/posts?q={q}");
            let app = &app;
            let admin = admin.clone();
            async move { ids(&app.get(&path, &admin).await.json()["posts"]) }
        };
        assert_eq!(search("sky").await, [d, c, b, a]);
        let related = |id: i64| {
            let app = &app;
            let admin = admin.clone();
            async move {
                ids(&app
                    .get(&format!("/api/v1/posts/{id}/related"), &admin)
                    .await
                    .json()["posts"])
            }
        };
        let before = related(c).await;
        assert!(before.contains(&a) && before.contains(&b) && before.contains(&d));

        // The entry is an alias; a parent hides its children.
        set_blacklist(&app, &admin, &["kitty", "footwear", "rating:explicit"]).await;
        assert_eq!(search("").await, [e, c]);
        assert_eq!(search("sky").await, [c]);
        // What the search names wins: the tag, through its alias, a child
        // of a hidden parent, the parent itself, the rating.
        assert_eq!(search("cat").await, [a]);
        assert_eq!(search("kitty").await, [a]);
        assert_eq!(search("boots").await, [b]);
        assert_eq!(search("footwear").await, [b]);
        assert_eq!(search("rating:explicit").await, [d]);
        // Only what it names: the rest of the list still applies.
        assert_eq!(search("sky+boots").await, [b]);
        assert_eq!(search("rare").await, [c]);
        // The recycle bin shows everything it holds.
        app.call(
            Method::DELETE,
            &format!("/api/v1/posts/{a}"),
            Some(&admin),
            None,
        )
        .await;
        assert_eq!(search("status:deleted").await, [a]);
        app.call(
            Method::POST,
            &format!("/api/v1/posts/{a}/restore"),
            Some(&admin),
            None,
        )
        .await;

        // Neighbours step over hidden posts.
        let n = app
            .get(&format!("/api/v1/posts/{c}/neighbors?q=sky"), &admin)
            .await
            .json();
        assert_eq!(n, json!({ "prev": null, "next": null }));
        let n = app
            .get(&format!("/api/v1/posts/{b}/neighbors?q=boots"), &admin)
            .await
            .json();
        assert_eq!(n, json!({ "prev": null, "next": null }));
        let n = app
            .get(&format!("/api/v1/posts/{e}/neighbors"), &admin)
            .await
            .json();
        assert_eq!(n, json!({ "prev": null, "next": c }));

        // Related posts and Explore.
        let after = related(c).await;
        assert!(
            !after.contains(&a) && !after.contains(&b) && !after.contains(&d),
            "{after:?}"
        );
        let explore = app.get("/api/v1/explore", &admin).await.json();
        let mut random = ids(&explore["random"]);
        random.sort_unstable();
        assert_eq!(random, [c, e]);
        let explore = app.get("/api/v1/explore?q=kitty", &admin).await.json();
        assert_eq!(ids(&explore["random"]), [a]);

        // The colour search: `a`'s own colour finds it only without the list.
        let palette = app.get(&format!("/api/v1/posts/{a}"), &admin).await.json()["palette"][0]
            .as_str()
            .unwrap()
            .trim_start_matches('#')
            .to_owned();
        assert!(!search(&format!("color:{palette}")).await.contains(&a));

        // A post opened by id opens, saying what it matches.
        let flags = |id: i64| {
            let app = &app;
            let admin = admin.clone();
            async move {
                let r = app.get(&format!("/api/v1/posts/{id}"), &admin).await;
                assert_eq!(r.status, StatusCode::OK);
                r.json()["blacklisted"].clone()
            }
        };
        assert_eq!(flags(a).await, json!(["cat"]));
        assert_eq!(flags(b).await, json!(["footwear"]));
        assert_eq!(flags(d).await, json!(["rating:explicit"]));
        assert_eq!(flags(c).await, Value::Null);

        set_blacklist(&app, &admin, &[]).await;
        assert_eq!(search("").await, [e, d, c, b, a]);
        assert!(search(&format!("color:{palette}")).await.contains(&a));
        assert_eq!(flags(a).await, Value::Null);
        app.finish().await;
    }

    /// Guests get the admin's default, or send their own in `exclude`; new
    /// accounts start from the default.
    #[tokio::test]
    async fn guests_and_new_accounts_follow_the_default() {
        let Some(app) = testing::app_with(|c| c.mode = Some(Mode::Public)).await else {
            return;
        };
        let admin = app.register("admin").await;
        let early = app.register("early").await;
        let a = app.upload_png(&admin, 1, "cat").await;
        let b = app.upload_png(&admin, 2, "dog").await;
        for id in [a, b] {
            app.call(
                Method::PUT,
                &format!("/api/v1/posts/{id}/vote"),
                Some(&early),
                Some(json!({ "value": 1 })),
            )
            .await;
        }
        let mut settings = app.get("/api/v1/admin/settings", &admin).await.json();
        assert_eq!(settings["default_blacklist"], json!([]));
        settings["default_blacklist"] = json!(["Cat"]);
        let saved = put_json(&app, &admin, "/api/v1/admin/settings", settings.clone()).await;
        assert_eq!(saved["default_blacklist"], json!(["cat"]));
        let mut long = settings.clone();
        long["default_blacklist"] = json!(
            (0..=MAX_EXCLUDE)
                .map(|i| format!("t{i}"))
                .collect::<Vec<_>>()
        );
        let r = app
            .call(
                Method::PUT,
                "/api/v1/admin/settings",
                Some(&admin),
                Some(long),
            )
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);

        let guest = |path: String| {
            let app = &app;
            async move {
                let r = app.call(Method::GET, &path, None, None).await;
                assert_eq!(r.status, StatusCode::OK, "{path}: {:?}", r.json());
                r.json()
            }
        };
        assert_eq!(
            guest("/api/v1/auth/blacklist".into()).await,
            json!({ "entries": ["cat"] })
        );
        assert_eq!(ids(&guest("/api/v1/posts".into()).await["posts"]), [b]);
        assert_eq!(
            ids(&guest("/api/v1/posts?exclude=".into()).await["posts"]),
            [b, a]
        );
        assert_eq!(
            ids(&guest("/api/v1/posts?exclude=dog+rating:e".into()).await["posts"]),
            [a]
        );
        assert_eq!(
            ids(&guest("/api/v1/posts?q=cat".into()).await["posts"]),
            [a]
        );
        let e = guest("/api/v1/explore".into()).await;
        assert_eq!(ids(&e["trending"]), [b]);
        let tags: Vec<&str> = e["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["tag"]["name"].as_str().unwrap())
            .collect();
        assert!(!tags.contains(&"cat") && tags.contains(&"dog"), "{tags:?}");
        let e = guest("/api/v1/explore?exclude=".into()).await;
        assert_eq!(ids(&e["trending"]).len(), 2);
        assert_eq!(
            guest(format!("/api/v1/posts/{a}")).await["blacklisted"],
            json!(["cat"])
        );
        assert!(guest(format!("/api/v1/posts/{a}?exclude=dog")).await["blacklisted"].is_null());
        let long = (0..=MAX_EXCLUDE)
            .map(|i| format!("t{i}"))
            .collect::<Vec<_>>()
            .join("+");
        let r = app
            .call(
                Method::GET,
                &format!("/api/v1/posts?exclude={long}"),
                None,
                None,
            )
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        let r = app
            .call(Method::GET, "/api/v1/posts?exclude=type:video", None, None)
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);

        // Accounts made from now on start from the default; earlier ones
        // keep theirs, and `exclude` is for guests only.
        let late = app.register("late").await;
        assert_eq!(
            app.get("/api/v1/auth/blacklist", &late).await.json()["entries"],
            json!(["cat"])
        );
        assert_eq!(
            app.get("/api/v1/auth/blacklist", &early).await.json()["entries"],
            json!([])
        );
        let page = app.get("/api/v1/posts?exclude=dog", &late).await.json();
        assert_eq!(ids(&page["posts"]), [b]);
        let page = app.get("/api/v1/posts?exclude=dog", &early).await.json();
        assert_eq!(ids(&page["posts"]), [b, a]);
        app.finish().await;
    }
}
