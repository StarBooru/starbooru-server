//! Link previews (`api.md` › *Link previews*): what a link's page says
//! about it in its head, and the pictures those tags point at.
//!
//! A share link has a preview on a private server whose admin allows them,
//! when it is live and it or its owner says yes. A post's own address has
//! one on a public server whose admin turned them on, while the post is
//! out of the bin. Both are checked on every request, so a change stops
//! them at once. Nothing here counts a view.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use axum::response::Response;
use sqlx::FromRow;
use starbooru_core::{MediaKind, Mode, ServerSettings, TagCategory, preview_on};

use crate::error::{AppError, AppResult};
use crate::previews::{
    self, Card, CardImage, Counts, Lang, Link, MAX_SHARE_IMAGES, Source, count_text,
    post_description, post_title,
};
use crate::settings;
use crate::state::AppState;

/// A post as a preview needs it.
#[derive(FromRow)]
struct PreviewPost {
    kind: i16,
    ext: String,
    sha256: Vec<u8>,
    width: Option<i32>,
    height: Option<i32>,
    has_thumb: bool,
    has_sample: Option<bool>,
}

impl PreviewPost {
    fn kind(&self) -> MediaKind {
        MediaKind::from_db(self.kind).unwrap_or(MediaKind::Image)
    }

    /// Where its preview comes from and its size; `None` for no preview.
    fn picture(&self, tools: bool) -> Option<(Source, (u32, u32))> {
        let source = previews::source(
            self.kind(),
            &self.ext,
            self.has_sample,
            self.has_thumb,
            tools,
        )?;
        Some((source, previews::size(self.width, self.height, source)?))
    }
}

/// A live link: its title, posts and choices.
#[derive(FromRow)]
struct LiveShare {
    title: Option<String>,
    post_ids: Vec<i64>,
    preview: Option<bool>,
    share_previews: bool,
}

/// The link with `token` if it shows a preview now, with its posts out of
/// the bin, in its order. Read only: a view isn't counted.
async fn shown_share(
    state: &AppState,
    settings: &ServerSettings,
    token: &str,
) -> AppResult<Option<(LiveShare, Vec<PreviewPost>)>> {
    let allowed = settings.mode == Mode::Private && settings.link_previews == Some(true);
    if !allowed || !starbooru_core::share::is_share_token(token) {
        return Ok(None);
    }
    let link: Option<LiveShare> = sqlx::query_as(
        "SELECT s.title, s.post_ids, s.preview, o.share_previews \
         FROM shares s JOIN users o ON o.id = s.user_id \
         WHERE s.token = $1 AND o.disabled_at IS NULL \
           AND (s.expires_at IS NULL OR s.expires_at > now())",
    )
    .bind(token)
    .fetch_optional(&state.db)
    .await?;
    let Some(link) = link.filter(|l| preview_on(true, l.share_previews, l.preview)) else {
        return Ok(None);
    };
    let posts: Vec<PreviewPost> = sqlx::query_as(
        "SELECT p.kind, p.ext, p.sha256, p.width, p.height, p.has_thumb, p.has_sample \
         FROM unnest($1::bigint[]) WITH ORDINALITY AS u(id, n) \
         JOIN posts p ON p.id = u.id WHERE p.deleted_at IS NULL ORDER BY u.n",
    )
    .bind(&link.post_ids)
    .fetch_all(&state.db)
    .await?;
    Ok(Some((link, posts)))
}

/// The post with `id` if its address shows a preview now.
async fn shown_post(
    state: &AppState,
    settings: &ServerSettings,
    id: i64,
) -> AppResult<Option<PreviewPost>> {
    if settings.mode != Mode::Public || settings.link_previews != Some(true) {
        return Ok(None);
    }
    Ok(sqlx::query_as(
        "SELECT p.kind, p.ext, p.sha256, p.width, p.height, p.has_thumb, p.has_sample \
         FROM posts p WHERE p.id = $1 AND p.deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?)
}

/// The tags for the head of a link's page: none when it has no preview,
/// or its absolute address can't be worked out.
pub(crate) async fn link_head(
    state: &AppState,
    link: &Link,
    headers: &HeaderMap,
    authority: Option<&str>,
) -> AppResult<String> {
    let config = &state.config;
    let Some(base) = previews::base_url(
        config.public_url.as_ref(),
        config.trust_proxy,
        headers,
        authority,
    ) else {
        return Ok(String::new());
    };
    let lang = previews::language(
        headers
            .get(header::ACCEPT_LANGUAGE)
            .and_then(|v| v.to_str().ok()),
    );
    let settings = settings::current(state).await?;
    let card = match link {
        Link::Share(token) => share_card(state, &settings, token, &base, lang).await?,
        Link::Post(id) => post_card(state, &settings, *id, &base).await?,
    };
    Ok(card.as_ref().map(previews::render).unwrap_or_default())
}

async fn share_card(
    state: &AppState,
    settings: &ServerSettings,
    token: &str,
    base: &str,
    lang: Lang,
) -> AppResult<Option<Card>> {
    let Some((link, posts)) = shown_share(state, settings, token).await? else {
        return Ok(None);
    };
    let mut counts = Counts::default();
    for post in &posts {
        counts.add(post.kind());
    }
    let title = link.title.unwrap_or_else(|| settings.name.clone());
    let tools = state.storage.has_tools();
    let images = posts
        .iter()
        .filter_map(|post| Some((post, post.picture(tools)?.1)))
        .take(MAX_SHARE_IMAGES)
        .enumerate()
        .map(|(n, (post, (width, height)))| CardImage {
            url: format!(
                "{base}preview/s/{token}/{n}.jpg?v={}",
                previews::version(&post.sha256)
            ),
            width,
            height,
            alt: title.clone(),
        })
        .collect();
    Ok(Some(Card {
        site: settings.name.clone(),
        description: count_text(lang, counts).unwrap_or_else(|| settings.name.clone()),
        title,
        url: format!("{base}s/{token}"),
        images,
    }))
}

async fn post_card(
    state: &AppState,
    settings: &ServerSettings,
    id: i64,
    base: &str,
) -> AppResult<Option<Card>> {
    let Some(post) = shown_post(state, settings, id).await? else {
        return Ok(None);
    };
    // The best known first, for the title.
    let tags: Vec<(String, i16)> = sqlx::query_as(
        "SELECT t.name, t.category FROM posts p JOIN tags t ON t.id = ANY(p.tag_ids) \
         WHERE p.id = $1 ORDER BY t.post_count DESC, t.name",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let of = |category: TagCategory| -> Vec<String> {
        tags.iter()
            .filter(|(_, c)| TagCategory::from_db(*c) == Some(category))
            .map(|(name, _)| name.clone())
            .collect()
    };
    let title = post_title(
        &of(TagCategory::Character),
        &of(TagCategory::Copyright),
        &of(TagCategory::Artist),
        id,
    );
    let mut general = of(TagCategory::General);
    general.sort();
    let images = post
        .picture(state.storage.has_tools())
        .map(|(_, (width, height))| CardImage {
            url: format!(
                "{base}preview/p/{id}.jpg?v={}",
                previews::version(&post.sha256)
            ),
            width,
            height,
            alt: title.clone(),
        })
        .into_iter()
        .collect();
    Ok(Some(Card {
        site: settings.name.clone(),
        description: post_description(&general, &settings.name),
        title,
        url: format!("{base}post/{id}"),
        images,
    }))
}

/// `GET /preview/s/{token}/{n}.jpg`: the picture of the link's `n`th post
/// (0 to 3) among those that have one. `?v=` is only for caches.
pub async fn share_image(
    State(state): State<AppState>,
    Path((token, file)): Path<(String, String)>,
) -> AppResult<Response> {
    let n = file
        .strip_suffix(".jpg")
        .filter(|n| n.len() == 1)
        .and_then(|n| n.parse::<usize>().ok())
        .filter(|n| *n < MAX_SHARE_IMAGES)
        .ok_or(AppError::NotFound)?;
    let settings = settings::current(&state).await?;
    let (_, posts) = shown_share(&state, &settings, &token)
        .await?
        .ok_or(AppError::NotFound)?;
    let tools = state.storage.has_tools();
    let post = posts
        .into_iter()
        .filter(|post| post.picture(tools).is_some())
        .nth(n)
        .ok_or(AppError::NotFound)?;
    let mut response = picture(&state, post).await?;
    response.headers_mut().insert(
        HeaderName::from_static("x-robots-tag"),
        HeaderValue::from_static("noindex"),
    );
    Ok(response)
}

/// `GET /preview/p/{id}.jpg`: a post's picture on a public server.
pub async fn post_image(
    State(state): State<AppState>,
    Path(file): Path<String>,
) -> AppResult<Response> {
    let id = file
        .strip_suffix(".jpg")
        .filter(|id| !id.is_empty() && id.len() <= 18 && id.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|id| id.parse::<i64>().ok())
        .ok_or(AppError::NotFound)?;
    let settings = settings::current(&state).await?;
    let post = shown_post(&state, &settings, id)
        .await?
        .ok_or(AppError::NotFound)?;
    picture(&state, post).await
}

/// The post's preview, made the first time (two at a time) and then kept.
async fn picture(state: &AppState, post: PreviewPost) -> AppResult<Response> {
    let tools = state.storage.has_tools();
    let (source, size) = post.picture(tools).ok_or(AppError::NotFound)?;
    let sha = hex::encode(&post.sha256);
    let kind = post.kind();
    let kept = {
        let (state, sha) = (state.clone(), sha.clone());
        tokio::task::spawn_blocking(move || state.storage.read_preview(&sha, size)).await?
    };
    let jpeg = match kept {
        Some(jpeg) => jpeg,
        None => {
            let _turn = state.previews.acquire().await.map_err(AppError::internal)?;
            let state = state.clone();
            tokio::task::spawn_blocking(move || {
                state.storage.preview(&sha, &post.ext, kind, source, size)
            })
            .await??
            .ok_or(AppError::NotFound)?
        }
    };
    let mut response = Response::new(Body::from(jpeg));
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=3600"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use serde_json::{Value, json};
    use starbooru_core::Mode;

    use crate::testing::{self, Resp, TestApp};

    const PAGE: &str =
        "<!DOCTYPE html><html><head><title>StarBooru</title></head><body></body></html>";

    /// A folder with just a page, for the server to serve.
    fn web() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), PAGE).unwrap();
        dir
    }

    async fn open(
        app: &TestApp,
        method: Method,
        path: &str,
        host: &str,
        lang: Option<&str>,
    ) -> Resp {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("host", host)
            .header("user-agent", "WhatsApp/2.23.20.0 A");
        if let Some(lang) = lang {
            req = req.header("accept-language", lang);
        }
        app.send(req.body(Body::empty()).unwrap()).await
    }

    async fn page(app: &TestApp, path: &str) -> String {
        let r = open(app, Method::GET, path, "booru.example.com", None).await;
        assert_eq!(r.status, StatusCode::OK, "{path}");
        assert_eq!(r.headers["cache-control"], "no-cache");
        String::from_utf8(r.body).unwrap()
    }

    /// The `content` of each `<meta property=…>` with this name, in order.
    fn meta(html: &str, property: &str) -> Vec<String> {
        let needle = format!("<meta property=\"{property}\" content=\"");
        html.match_indices(&needle)
            .map(|(at, _)| {
                let rest = &html[at + needle.len()..];
                rest[..rest.find('"').unwrap()].to_owned()
            })
            .collect()
    }

    async fn set_previews(app: &TestApp, admin: &str, on: Option<bool>) -> Value {
        let mut settings = app.get("/api/v1/admin/settings", admin).await.json();
        let map = settings.as_object_mut().unwrap();
        match on {
            Some(on) => map.insert("link_previews".into(), json!(on)),
            None => map.remove("link_previews"),
        };
        let r = app
            .call(
                Method::PUT,
                "/api/v1/admin/settings",
                Some(admin),
                Some(settings),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        r.json()
    }

    fn jpeg_size(body: &[u8]) -> (u32, u32) {
        let img = image::load_from_memory_with_format(body, image::ImageFormat::Jpeg).unwrap();
        (img.width(), img.height())
    }

    fn stored_previews(app: &TestApp) -> usize {
        let root = app.state.config.data_dir.join("preview");
        walk(&root)
    }

    fn walk(dir: &std::path::Path) -> usize {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        entries
            .map(|e| e.unwrap().path())
            .map(|p| if p.is_dir() { walk(&p) } else { 1 })
            .sum()
    }

    #[tokio::test]
    async fn a_share_link_shows_its_pictures_when_everyone_allows_it() {
        let web = web();
        let dir = web.path().to_path_buf();
        let Some(app) = testing::app_with(move |c| c.web_dir = Some(dir)).await else {
            return;
        };
        let admin = app.register("admin").await;
        let bob = app.register("bob").await;
        let small = app.upload_png(&bob, 1, "").await;
        let r = app
            .upload(&bob, &testing::textured_png(2000, 1500, 7), &[])
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        let big = r.json();
        assert!(big["sample_url"].is_string(), "a big picture gets a sample");
        let big = big["id"].as_i64().unwrap();
        let r = app.upload(&bob, &testing::gif(30, 20), &[]).await;
        let gif = r.json()["id"].as_i64().unwrap();
        let r = app
            .call(
                Method::POST,
                "/api/v1/shares",
                Some(&bob),
                Some(json!({ "post_ids": [small, big, gif], "title": "Trip \"&\" <co>" })),
            )
            .await;
        let share = r.json();
        assert_eq!(
            (share["preview"].clone(), share["preview_on"].clone()),
            (json!(null), json!(false))
        );
        let token = share["token"].as_str().unwrap().to_owned();
        let id = share["id"].as_i64().unwrap();
        let path = format!("/s/{token}");
        let image = |n: usize| format!("/preview/s/{token}/{n}.jpg?v=0");

        // The admin's switch is off: only the robots tag.
        let r = open(&app, Method::GET, &path, "booru.example.com", None).await;
        assert_eq!(r.headers["x-robots-tag"], "noindex, nofollow");
        assert_eq!(r.headers["vary"], "accept-language");
        let html = String::from_utf8(r.body).unwrap();
        assert!(html.starts_with(
            "<!DOCTYPE html><html><head>\n<meta name=\"robots\" content=\"noindex, nofollow\">"
        ));
        assert!(meta(&html, "og:title").is_empty());
        assert_eq!(
            open(&app, Method::GET, &image(0), "booru.example.com", None)
                .await
                .status,
            404
        );
        let info = app
            .call(Method::GET, "/api/v1/info", None, None)
            .await
            .json();
        assert_eq!(info["link_previews"], false);
        assert_eq!(
            app.get("/api/v1/admin/settings", &admin).await.json()["link_previews"],
            false
        );

        // On, but neither bob nor the link has said yes.
        assert_eq!(
            set_previews(&app, &admin, Some(true)).await["link_previews"],
            true
        );
        assert_eq!(
            app.call(Method::GET, "/api/v1/info", None, None)
                .await
                .json()["link_previews"],
            true
        );
        assert!(meta(&page(&app, &path).await, "og:title").is_empty());
        assert_eq!(
            app.get("/api/v1/shares", &bob).await.json()[0]["preview_on"],
            false
        );

        // Bob's default says yes, and the link follows it.
        let r = app
            .call(
                Method::PUT,
                "/api/v1/auth/sharing",
                Some(&bob),
                Some(json!({ "previews": true })),
            )
            .await;
        assert_eq!(r.json(), json!({ "previews": true }));
        let listed = app.get("/api/v1/shares", &bob).await.json();
        assert_eq!(
            (
                listed[0]["preview"].clone(),
                listed[0]["preview_on"].clone()
            ),
            (json!(null), json!(true))
        );
        let html = page(&app, &path).await;
        assert_eq!(meta(&html, "og:type"), ["website"]);
        assert_eq!(meta(&html, "og:site_name"), ["StarBooru"]);
        assert_eq!(
            meta(&html, "og:title"),
            ["Trip &quot;&amp;&quot; &lt;co&gt;"]
        );
        assert_eq!(
            meta(&html, "og:description"),
            ["2 pictures and 1 animation"]
        );
        assert_eq!(
            meta(&html, "og:url"),
            [format!("https://booru.example.com/s/{token}")]
        );
        let images = meta(&html, "og:image");
        assert_eq!(images.len(), 3);
        for (n, url) in images.iter().enumerate() {
            assert!(
                url.starts_with(&format!(
                    "https://booru.example.com/preview/s/{token}/{n}.jpg?v="
                )),
                "{url}"
            );
        }
        // The small PNG as it is, the sample of the big one fitted in 1200, the GIF.
        assert_eq!(meta(&html, "og:image:width"), ["9", "1200", "30"]);
        assert_eq!(meta(&html, "og:image:height"), ["8", "900", "20"]);
        assert_eq!(meta(&html, "og:image:type"), ["image/jpeg"; 3]);
        assert!(html.contains("<meta name=\"twitter:card\" content=\"summary_large_image\">"));
        // The tags come before anything else in the head.
        assert!(html.find("og:type").unwrap() < html.find("<title>").unwrap());
        let r = open(
            &app,
            Method::GET,
            &path,
            "booru.example.com",
            Some("it-IT,it;q=0.9,en;q=0.8"),
        )
        .await;
        assert_eq!(
            meta(&String::from_utf8(r.body).unwrap(), "og:description"),
            ["2 immagini e 1 animazione"]
        );

        // The pictures, at the size the tags say.
        let r = open(&app, Method::GET, &image(1), "booru.example.com", None).await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(r.headers["content-type"], "image/jpeg");
        assert_eq!(r.headers["cache-control"], "public, max-age=3600");
        assert_eq!(r.headers["x-content-type-options"], "nosniff");
        assert_eq!(r.headers["x-robots-tag"], "noindex");
        assert!(r.body.len() < 600 * 1000, "{} bytes", r.body.len());
        assert_eq!(jpeg_size(&r.body), (1200, 900));
        let first = r.body;
        let again = open(
            &app,
            Method::GET,
            &format!("/preview/s/{token}/1.jpg"),
            "x.example",
            None,
        )
        .await;
        assert_eq!(again.body, first, "made once, then kept");
        assert_eq!(
            jpeg_size(
                &open(&app, Method::GET, &image(0), "x.example", None)
                    .await
                    .body
            ),
            (9, 8)
        );
        assert_eq!(
            jpeg_size(
                &open(&app, Method::GET, &image(2), "x.example", None)
                    .await
                    .body
            ),
            (30, 20)
        );
        assert_eq!(stored_previews(&app), 3);
        let head = open(&app, Method::HEAD, &image(1), "x.example", None).await;
        assert_eq!((head.status, head.body.len()), (StatusCode::OK, 0));
        for missing in [
            image(3),
            image(4),
            format!("/preview/s/{token}/01.jpg"),
            format!("/preview/s/{token}/x.jpg"),
            format!("/preview/s/{token}/1.png"),
            "/preview/s/0123456789abcdef0123456789abcdef/0.jpg".to_owned(),
            "/preview/s/nope/0.jpg".to_owned(),
        ] {
            let r = open(&app, Method::GET, &missing, "x.example", None).await;
            assert_eq!(r.status, StatusCode::NOT_FOUND, "{missing}");
        }

        // Nothing here counts as a view.
        assert_eq!(app.get("/api/v1/shares", &bob).await.json()[0]["views"], 0);

        // The link says no, then nothing (it stays no), then follows bob again.
        let patch = |body: Value| {
            let (app, bob) = (&app, bob.clone());
            async move {
                app.call(
                    Method::PATCH,
                    &format!("/api/v1/shares/{id}"),
                    Some(&bob),
                    Some(body),
                )
                .await
            }
        };
        let r = patch(json!({ "preview": false })).await;
        assert_eq!(
            (r.json()["preview"].clone(), r.json()["preview_on"].clone()),
            (json!(false), json!(false))
        );
        assert_eq!(r.json()["token"], token.as_str());
        assert!(meta(&page(&app, &path).await, "og:title").is_empty());
        assert_eq!(
            open(&app, Method::GET, &image(0), "x.example", None)
                .await
                .status,
            404
        );
        let r = patch(json!({})).await;
        assert_eq!(r.json()["preview"], false);
        let r = patch(json!({ "preview": null })).await;
        assert_eq!(
            (r.json()["preview"].clone(), r.json()["preview_on"].clone()),
            (json!(null), json!(true))
        );
        // Bob says no by default; the link can still say yes on its own.
        app.call(
            Method::PUT,
            "/api/v1/auth/sharing",
            Some(&bob),
            Some(json!({ "previews": false })),
        )
        .await;
        assert!(meta(&page(&app, &path).await, "og:title").is_empty());
        let r = patch(json!({ "preview": true })).await;
        assert_eq!(r.json()["preview_on"], true);
        assert_eq!(meta(&page(&app, &path).await, "og:image").len(), 3);
        // Only the owner changes a link.
        let r = app
            .call(
                Method::PATCH,
                &format!("/api/v1/shares/{id}"),
                Some(&admin),
                Some(json!({ "preview": false })),
            )
            .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
        let r = app
            .call(
                Method::PATCH,
                "/api/v1/shares/999999",
                Some(&bob),
                Some(json!({ "preview": false })),
            )
            .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);

        // A binned post drops out at once, and the pictures move up.
        app.call(
            Method::DELETE,
            &format!("/api/v1/posts/{big}"),
            Some(&bob),
            None,
        )
        .await;
        let html = page(&app, &path).await;
        assert_eq!(meta(&html, "og:description"), ["1 picture and 1 animation"]);
        assert_eq!(meta(&html, "og:image:width"), ["9", "30"]);
        assert_eq!(
            jpeg_size(
                &open(&app, Method::GET, &image(1), "x.example", None)
                    .await
                    .body
            ),
            (30, 20)
        );
        assert_eq!(
            open(&app, Method::GET, &image(2), "x.example", None)
                .await
                .status,
            404
        );
        app.call(
            Method::POST,
            &format!("/api/v1/posts/{big}/restore"),
            Some(&bob),
            None,
        )
        .await;
        assert_eq!(meta(&page(&app, &path).await, "og:image").len(), 3);

        // Expired: the robots tag only, and no pictures.
        let expire = |when: &'static str| {
            let app = &app;
            async move {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "UPDATE shares SET expires_at = {when} WHERE id = $1"
                )))
                .bind(id)
                .execute(app.db())
                .await
                .unwrap();
            }
        };
        expire("now() - interval '1 minute'").await;
        let html = page(&app, &path).await;
        assert!(html.contains("noindex, nofollow") && meta(&html, "og:title").is_empty());
        assert_eq!(
            open(&app, Method::GET, &image(0), "x.example", None)
                .await
                .status,
            404
        );
        expire("NULL").await;
        assert_eq!(
            open(&app, Method::GET, &image(0), "x.example", None)
                .await
                .status,
            200
        );

        // A disabled owner's links show nothing.
        let bob_id = app.get("/api/v1/auth/me", &bob).await.json()["id"].clone();
        let disable = |disabled: bool| {
            let (app, admin) = (&app, admin.clone());
            let user = format!("/api/v1/admin/users/{bob_id}");
            async move {
                app.call(
                    Method::PATCH,
                    &user,
                    Some(&admin),
                    Some(json!({ "disabled": disabled })),
                )
                .await
                .status
            }
        };
        assert_eq!(disable(true).await, 200);
        assert!(meta(&page(&app, &path).await, "og:title").is_empty());
        assert_eq!(
            open(&app, Method::GET, &image(0), "x.example", None)
                .await
                .status,
            404
        );
        assert_eq!(disable(false).await, 200);
        assert_eq!(meta(&page(&app, &path).await, "og:title").len(), 1);
        // Disabling ended bob's sessions.
        let r = app
            .call(
                Method::POST,
                "/api/v1/auth/login",
                None,
                Some(json!({ "username": "bob", "password": testing::PASSWORD })),
            )
            .await;
        let bob = r.json()["token"].as_str().unwrap().to_owned();

        // A host that isn't a plain name: no address to give, so no preview.
        let r = open(&app, Method::GET, &path, "booru.example.com/evil", None).await;
        let html = String::from_utf8(r.body).unwrap();
        assert!(html.contains("noindex") && meta(&html, "og:title").is_empty());
        let r = open(&app, Method::GET, &path, "127.0.0.1:8120", None).await;
        let html = String::from_utf8(r.body).unwrap();
        assert_eq!(
            meta(&html, "og:url"),
            [format!("http://127.0.0.1:8120/s/{token}")]
        );

        // Left out of a PUT, the switch stays; turned off, everything stops.
        assert_eq!(
            set_previews(&app, &admin, None).await["link_previews"],
            true
        );
        assert_eq!(
            set_previews(&app, &admin, Some(false)).await["link_previews"],
            false
        );
        assert!(meta(&page(&app, &path).await, "og:title").is_empty());
        assert_eq!(
            open(&app, Method::GET, &image(0), "x.example", None)
                .await
                .status,
            404
        );
        assert_eq!(
            app.get("/api/v1/shares", &bob).await.json()[0]["preview_on"],
            false
        );
        set_previews(&app, &admin, Some(true)).await;

        // Removed: gone at once.
        app.call(
            Method::DELETE,
            &format!("/api/v1/shares/{id}"),
            Some(&bob),
            None,
        )
        .await;
        let html = page(&app, &path).await;
        assert!(html.contains("noindex") && meta(&html, "og:title").is_empty());
        assert_eq!(
            open(&app, Method::GET, &image(0), "x.example", None)
                .await
                .status,
            404
        );

        // A link made with a choice keeps it; other pages carry no robots tag.
        let r = app
            .call(
                Method::POST,
                "/api/v1/shares",
                Some(&bob),
                Some(json!({ "post_ids": [small], "preview": true })),
            )
            .await;
        assert_eq!(
            (r.json()["preview"].clone(), r.json()["preview_on"].clone()),
            (json!(true), json!(true))
        );
        for other in ["/", "/settings", "/s/nope", &format!("/post/{small}")] {
            let r = open(&app, Method::GET, other, "booru.example.com", None).await;
            assert!(r.headers.get("x-robots-tag").is_none(), "{other}");
            assert!(
                !String::from_utf8(r.body).unwrap().contains("og:"),
                "{other}"
            );
        }
        app.finish().await;
    }

    #[tokio::test]
    async fn a_posts_address_shows_it_on_a_public_server() {
        let web = web();
        let dir = web.path().to_path_buf();
        let Some(app) = testing::app_with(move |c| {
            c.web_dir = Some(dir);
            c.mode = Some(Mode::Public);
            c.public_url = Some(url::Url::parse("https://pics.example.org/booru/").unwrap());
        })
        .await
        else {
            return;
        };
        let admin = app.register("admin").await;
        let r = app
            .upload(
                &admin,
                &testing::textured_png(900, 3000, 3),
                &[
                    (
                        "tags",
                        "char:hatsune_miku copy:vocaloid artist:someone blue_sky night_sky",
                    ),
                    ("rating", "explicit"),
                ],
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        let id = r.json()["id"].as_i64().unwrap();
        let bare = app.upload_png(&admin, 5, "").await;
        let path = format!("/post/{id}");
        let image = format!("/preview/p/{id}.jpg");

        // Off by default.
        let html = page(&app, &path).await;
        assert!(meta(&html, "og:title").is_empty() && !html.contains("robots"));
        assert_eq!(
            open(&app, Method::GET, &image, "x.example", None)
                .await
                .status,
            404
        );

        set_previews(&app, &admin, Some(true)).await;
        let r = open(
            &app,
            Method::GET,
            &format!("{path}?q=night"),
            "evil.example",
            None,
        )
        .await;
        assert!(r.headers.get("x-robots-tag").is_none());
        let html = String::from_utf8(r.body).unwrap();
        assert_eq!(meta(&html, "og:title"), ["hatsune miku"]);
        assert_eq!(meta(&html, "og:description"), ["blue sky, night sky"]);
        // The setting's address, not the request's.
        assert_eq!(
            meta(&html, "og:url"),
            [format!("https://pics.example.org/booru/post/{id}")]
        );
        let url = &meta(&html, "og:image")[0];
        assert!(
            url.starts_with(&format!(
                "https://pics.example.org/booru/preview/p/{id}.jpg?v="
            )),
            "{url}"
        );
        // A tall picture fits in 1200 by its height.
        assert_eq!(
            (
                meta(&html, "og:image:width"),
                meta(&html, "og:image:height")
            ),
            (vec!["360".to_owned()], vec!["1200".to_owned()])
        );
        assert_eq!(meta(&html, "og:image:alt"), ["hatsune miku"]);
        let r = open(
            &app,
            Method::GET,
            &format!("{image}?v=whatever"),
            "x.example",
            None,
        )
        .await;
        assert_eq!(r.status, StatusCode::OK);
        assert!(r.headers.get("x-robots-tag").is_none());
        assert_eq!(jpeg_size(&r.body), (360, 1200));
        // No tags at all: the number, and the server's name.
        let html = page(&app, &format!("/post/{bare}")).await;
        assert_eq!(meta(&html, "og:title"), [format!("#{bare}")]);
        assert_eq!(meta(&html, "og:description"), ["StarBooru"]);
        for missing in [
            "/preview/p/x.jpg",
            "/preview/p/999999.jpg",
            "/preview/p/1.png",
            "/preview/p/.jpg",
        ] {
            assert_eq!(
                open(&app, Method::GET, missing, "x.example", None)
                    .await
                    .status,
                404,
                "{missing}"
            );
        }

        // Binned: nothing; restored: back.
        app.call(
            Method::DELETE,
            &format!("/api/v1/posts/{id}"),
            Some(&admin),
            None,
        )
        .await;
        assert!(meta(&page(&app, &path).await, "og:title").is_empty());
        assert_eq!(
            open(&app, Method::GET, &image, "x.example", None)
                .await
                .status,
            404
        );
        app.call(
            Method::POST,
            &format!("/api/v1/posts/{id}/restore"),
            Some(&admin),
            None,
        )
        .await;
        assert_eq!(
            open(&app, Method::GET, &image, "x.example", None)
                .await
                .status,
            200
        );
        assert_eq!(stored_previews(&app), 1);

        // Links aren't for a public server: PATCH is a 404 like the rest.
        let r = app
            .call(
                Method::PATCH,
                "/api/v1/shares/1",
                Some(&admin),
                Some(json!({ "preview": true })),
            )
            .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
        assert_eq!(
            app.get("/api/v1/auth/sharing", &admin).await.json(),
            json!({ "previews": false })
        );

        // Purged: its preview goes with its other files.
        app.call(
            Method::DELETE,
            &format!("/api/v1/posts/{id}"),
            Some(&admin),
            None,
        )
        .await;
        let r = app
            .call(
                Method::POST,
                &format!("/api/v1/posts/{id}/purge"),
                Some(&admin),
                None,
            )
            .await;
        assert_eq!(r.status, StatusCode::NO_CONTENT);
        assert_eq!(stored_previews(&app), 0);

        // A private server shows posts' addresses to no one.
        app.switch(&admin, "private").await;
        let html = page(&app, &format!("/post/{bare}")).await;
        assert!(meta(&html, "og:title").is_empty());
        assert_eq!(
            open(
                &app,
                Method::GET,
                &format!("/preview/p/{bare}.jpg"),
                "x.example",
                None
            )
            .await
            .status,
            404
        );
        app.finish().await;
    }

    #[tokio::test]
    async fn sharing_defaults_belong_to_each_account() {
        let Some(app) = testing::app().await else {
            return;
        };
        let alice = app.register("alice").await;
        let bob = app.register("bob").await;
        let sharing = "/api/v1/auth/sharing";
        assert_eq!(
            app.get(sharing, &alice).await.json(),
            json!({ "previews": false })
        );
        assert_eq!(app.call(Method::GET, sharing, None, None).await.status, 401);
        let r = app
            .call(
                Method::PUT,
                sharing,
                None,
                Some(json!({ "previews": true })),
            )
            .await;
        assert_eq!(r.status, 401);
        let r = app
            .call(
                Method::PUT,
                sharing,
                Some(&alice),
                Some(json!({ "previews": true })),
            )
            .await;
        assert_eq!(r.json(), json!({ "previews": true }));
        assert_eq!(
            app.get(sharing, &alice).await.json(),
            json!({ "previews": true })
        );
        assert_eq!(
            app.get(sharing, &bob).await.json(),
            json!({ "previews": false })
        );
        // Keys read it; a read-only one can't change it.
        let key = |read_only: bool| {
            let (app, alice) = (&app, alice.clone());
            async move {
                app.call(
                    Method::POST,
                    "/api/v1/keys",
                    Some(&alice),
                    Some(json!({ "name": format!("k{read_only}"), "read_only": read_only })),
                )
                .await
                .json()["token"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            }
        };
        let viewer = key(true).await;
        assert_eq!(
            app.get(sharing, &viewer).await.json(),
            json!({ "previews": true })
        );
        let r = app
            .call(
                Method::PUT,
                sharing,
                Some(&viewer),
                Some(json!({ "previews": false })),
            )
            .await;
        assert_eq!(r.status, 403);
        let full = key(false).await;
        let r = app
            .call(
                Method::PUT,
                sharing,
                Some(&full),
                Some(json!({ "previews": false })),
            )
            .await;
        assert_eq!(r.json(), json!({ "previews": false }));
        app.finish().await;
    }
}
