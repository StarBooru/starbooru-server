//! People without an account on a public instance: what they may read, and
//! how often. On a private instance nothing here applies, and every endpoint
//! but the open ones answers 401 without a token, as always.

use axum::extract::{FromRequestParts, MatchedPath, Request, State};
use axum::http::Method;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use chrono::{TimeDelta, Utc};

use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::settings;
use crate::state::AppState;
use crate::throttle::{ClientAddr, Throttle};

/// What people without an account may read on a public instance: `GET` (and
/// `HEAD`) of these routes, under `/api/v1`, each with what it costs of
/// [`GUEST_REQUESTS`]. Their handlers take a [`Viewer`]. `/info`, signing
/// up and in, and media are open in both modes anyway.
pub const GUEST_READS: &[(&str, u32)] = &[
    ("/posts", 1),
    ("/posts/{id}", 1),
    ("/posts/{id}/neighbors", 1),
    ("/posts/{id}/related", 1),
    ("/tags", 1),
    ("/tags/{name}", 1),
    ("/tag-corrections", 1),
    // Four sections, two of them the heaviest queries a guest can ask for.
    ("/explore", 5),
    ("/users/{name}", 1),
    ("/forum", 1),
    ("/forum/threads", 1),
    ("/forum/threads/{id}", 1),
];

/// What one client address may ask for without an account in a window.
/// Browsing quickly, a person makes a few requests a second: a page of
/// results, or a post with its neighbours and related posts.
pub const GUEST_REQUESTS: u32 = 300;
pub const GUEST_WINDOW: TimeDelta = TimeDelta::minutes(1);

pub fn throttle() -> Throttle {
    Throttle::new(GUEST_REQUESTS, GUEST_WINDOW)
}

/// Set on a request a guest may make, for [`Viewer`].
#[derive(Clone, Copy)]
struct GuestPass;

/// Lets a request without a token through to the routes of [`GUEST_READS`]
/// on a public instance, within the guests' limit per address. Requests
/// with a token, and everything else, pass untouched.
pub async fn gate(
    State(state): State<AppState>,
    ClientAddr(address): ClientAddr,
    mut req: Request,
    next: Next,
) -> Response {
    let cost = match guest_cost(&req) {
        Some(cost) if !req.headers().contains_key(AUTHORIZATION) => cost,
        _ => return next.run(req).await,
    };
    match settings::mode(&state).await {
        Ok(mode) if mode.guests() => {}
        Ok(_) => return next.run(req).await,
        Err(e) => return e.into_response(),
    }
    // Without an address (no socket, as in tests) every guest shares one.
    let key = address.unwrap_or_default();
    if let Err(retry_after) = state.guests.take(&key, Utc::now(), cost) {
        return AppError::GuestLimit { retry_after }.into_response();
    }
    req.extensions_mut().insert(GuestPass);
    next.run(req).await
}

/// What the request costs a guest, if a guest may make it.
fn guest_cost(req: &Request) -> Option<u32> {
    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        return None;
    }
    let path = req.extensions().get::<MatchedPath>()?.as_str();
    let path = path
        .strip_prefix(starbooru_core::API_PREFIX)
        .unwrap_or(path);
    GUEST_READS
        .iter()
        .find(|(read, _)| *read == path)
        .map(|(_, cost)| *cost)
}

/// Who is reading: someone signed in, or a guest on a public instance.
pub enum Viewer {
    User(AuthUser),
    Guest,
}

/// No account has this id, so a guest's own vote is always none.
const NO_ACCOUNT: i64 = 0;

impl Viewer {
    /// The account's id, or one no account has for a guest.
    pub fn id(&self) -> i64 {
        match self {
            Viewer::User(user) => user.id,
            Viewer::Guest => NO_ACCOUNT,
        }
    }

    pub fn is_guest(&self) -> bool {
        matches!(self, Viewer::Guest)
    }

    /// Refuses guests: the recycle bin, for one, needs an account.
    pub fn require_account(&self) -> AppResult<()> {
        if self.is_guest() {
            Err(AppError::Unauthorized)
        } else {
            Ok(())
        }
    }
}

impl FromRequestParts<AppState> for Viewer {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> AppResult<Self> {
        if parts.headers.contains_key(AUTHORIZATION) {
            return AuthUser::from_request_parts(parts, state)
                .await
                .map(Viewer::User);
        }
        if parts.extensions.get::<GuestPass>().is_some() {
            return Ok(Viewer::Guest);
        }
        Err(AppError::Unauthorized)
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use serde_json::json;
    use starbooru_core::Mode;

    use super::*;
    use crate::testing;

    /// What a guest gets for anything outside `GUEST_READS`.
    const REFUSED: StatusCode = StatusCode::UNAUTHORIZED;

    #[tokio::test]
    async fn guests_read_a_public_instance_only() {
        let Some(app) = testing::app().await else {
            return;
        };
        let admin = app.register("admin").await;
        let post = app.upload_png(&admin, 1, "blue_sky").await;
        let binned = app.upload_png(&admin, 2, "blue_sky").await;
        app.call(
            Method::DELETE,
            &format!("/api/v1/posts/{binned}"),
            Some(&admin),
            None,
        )
        .await;
        app.call(
            Method::PUT,
            &format!("/api/v1/posts/{post}/vote"),
            Some(&admin),
            Some(json!({ "value": 1 })),
        )
        .await;
        let reads = [
            "/api/v1/posts".to_owned(),
            format!("/api/v1/posts/{post}"),
            format!("/api/v1/posts/{post}/neighbors?q=blue_sky"),
            format!("/api/v1/posts/{post}/related"),
            "/api/v1/tags?q=blue".to_owned(),
            "/api/v1/tags/blue_sky".to_owned(),
            "/api/v1/tag-corrections?q=blue_skz".to_owned(),
            "/api/v1/explore".to_owned(),
        ];
        let closed = [
            (Method::GET, "/api/v1/auth/me".to_owned()),
            (Method::GET, "/api/v1/searches".to_owned()),
            (Method::GET, "/api/v1/shares".to_owned()),
            (Method::GET, "/api/v1/imports".to_owned()),
            (Method::GET, "/api/v1/duplicates".to_owned()),
            (Method::GET, "/api/v1/admin/stats".to_owned()),
            (Method::GET, format!("/api/v1/posts/{post}/translations")),
            (Method::PUT, format!("/api/v1/posts/{post}/vote")),
            (Method::PATCH, format!("/api/v1/posts/{post}")),
            (Method::PATCH, "/api/v1/tags/blue_sky".to_owned()),
            (Method::DELETE, format!("/api/v1/posts/{post}")),
            (Method::POST, "/api/v1/posts/bulk".to_owned()),
        ];

        // Private: a token for everything, as before.
        for path in &reads {
            let r = app.call(Method::GET, path, None, None).await;
            assert_eq!(r.status, REFUSED, "{path}");
        }
        let info = app.call(Method::GET, "/api/v1/info", None, None).await;
        assert_eq!(info.json()["guests"], false);

        // Public: reads open, the rest still closed.
        crate::settings::save_mode(app.db(), Mode::Public)
            .await
            .unwrap();
        let info = app.call(Method::GET, "/api/v1/info", None, None).await;
        assert_eq!(info.json()["guests"], true);
        for path in &reads {
            let r = app.call(Method::GET, path, None, None).await;
            assert_eq!(r.status, StatusCode::OK, "{path}: {:?}", r.json());
        }
        for (method, path) in &closed {
            let r = app
                .call(method.clone(), path, None, Some(json!({ "value": 1 })))
                .await;
            assert_eq!(r.status, REFUSED, "{method} {path}");
        }
        // A guest has no vote of their own and doesn't see the bin.
        let p = app
            .call(Method::GET, &format!("/api/v1/posts/{post}"), None, None)
            .await
            .json();
        assert_eq!(
            (p["score"].as_i64(), p["my_vote"].as_i64()),
            (Some(1), Some(0))
        );
        let mine = app
            .get(&format!("/api/v1/posts/{post}"), &admin)
            .await
            .json();
        assert_eq!(mine["my_vote"], 1);
        let r = app
            .call(Method::GET, &format!("/api/v1/posts/{binned}"), None, None)
            .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
        let r = app
            .call(
                Method::GET,
                &format!("/api/v1/posts/{binned}/related"),
                None,
                None,
            )
            .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
        for path in [
            "/api/v1/posts?q=status:deleted".to_owned(),
            format!("/api/v1/posts/{post}/neighbors?q=status:deleted"),
        ] {
            let r = app.call(Method::GET, &path, None, None).await;
            assert_eq!(r.status, REFUSED, "{path}");
        }
        let page = app
            .call(Method::GET, "/api/v1/posts", None, None)
            .await
            .json();
        assert_eq!(page["posts"].as_array().unwrap().len(), 1);
        // A bad token is still a 401, not a guest.
        let r = app.get("/api/v1/posts", "sb_nope").await;
        assert_eq!(r.status, REFUSED);
        // The admin's own bin still opens with a token.
        let r = app.get("/api/v1/posts?q=status:deleted", &admin).await;
        assert_eq!(r.json()["posts"][0]["id"], binned);
        app.finish().await;
    }

    #[tokio::test]
    async fn guests_are_held_to_a_limit_per_address() {
        let Some(app) = testing::app_with(|c| {
            c.mode = Some(Mode::Public);
            c.trust_proxy = true;
        })
        .await
        else {
            return;
        };
        let admin = app.register("admin").await;
        let from = |address: &'static str| {
            axum::http::Request::builder()
                .uri("/api/v1/tags")
                .header("x-forwarded-for", address)
                .body(axum::body::Body::empty())
                .unwrap()
        };
        for _ in 0..GUEST_REQUESTS {
            assert_eq!(app.send(from("198.51.100.4")).await.status, StatusCode::OK);
        }
        let r = app.send(from("198.51.100.4")).await;
        assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(r.json()["code"], "rate_limited");
        let wait: u64 = r.headers["retry-after"].to_str().unwrap().parse().unwrap();
        assert!((1..=60).contains(&wait), "{wait}");
        // Another address, and anyone signed in, carry on.
        assert_eq!(app.send(from("198.51.100.5")).await.status, StatusCode::OK);
        assert_eq!(app.get("/api/v1/tags", &admin).await.status, StatusCode::OK);
        // One IPv6 network counts as one address.
        for i in 0..GUEST_REQUESTS {
            let r = app
                .send(
                    axum::http::Request::builder()
                        .uri("/api/v1/tags")
                        .header("x-forwarded-for", format!("2001:db8:7:7::{:x}", i + 1))
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await;
            assert_eq!(r.status, StatusCode::OK);
        }
        let r = app.send(from("2001:db8:7:7:ffff::1")).await;
        assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
        // Explore costs more.
        let explore = || {
            axum::http::Request::builder()
                .uri("/api/v1/explore")
                .header("x-forwarded-for", "198.51.100.9")
                .body(axum::body::Body::empty())
                .unwrap()
        };
        let mut served = 0;
        while app.send(explore()).await.status == StatusCode::OK {
            served += 1;
        }
        assert_eq!(served, GUEST_REQUESTS / 5);
        app.finish().await;
    }
}
