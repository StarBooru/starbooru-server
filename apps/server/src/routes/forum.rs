//! The forum of a public instance (`api.md` › Forum): categories set by
//! admins, threads and posts. Guests read it; writing needs an account and
//! is held to a pace. On a private instance every endpoint answers 403
//! `forum_off`.

use axum::Json;
use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use sqlx::{FromRow, Postgres, QueryBuilder, Transaction};
use starbooru_core::forum::{
    DEFAULT_CATEGORIES, DEFAULT_POSTS_LIMIT, DEFAULT_THREADS_LIMIT, MAX_CATEGORIES,
    MAX_FORUM_LIMIT, POST_INTERVAL_SECS, POSTS_PER_DAY, clean_body, clean_category_name,
    clean_description, clean_title,
};
use starbooru_core::{
    CategoryUpdate, ForumCategory, ForumPost, ForumPostBody, ForumThread, Mode, NewCategory,
    NewThread, Role, ThreadPage, ThreadUpdate, ThreadView, UserCard,
};

use super::admin::require_admin_session;
use crate::auth::AuthUser;
use crate::avatars;
use crate::error::{AppError, AppResult};
use crate::guests::Viewer;
use crate::settings;
use crate::state::AppState;

const MAX_PAGE: u32 = 10_000;
/// Longest search, in characters.
const MAX_QUERY_CHARS: usize = 200;
const DAY_SECS: i64 = 24 * 60 * 60;

const OFF: &str = "forum_off";
const OFF_MESSAGE: &str = "a private server has no forum";

/// Answers 403 `forum_off` on a private instance.
pub async fn gate(State(state): State<AppState>, req: Request, next: Next) -> Response {
    match settings::mode(&state).await {
        Ok(Mode::Public) => next.run(req).await,
        Ok(Mode::Private) => AppError::Refused {
            code: OFF,
            message: OFF_MESSAGE,
        }
        .into_response(),
        Err(e) => e.into_response(),
    }
}

/// Starts a write, held public until it is saved: a switch to private
/// deletes threads and posts, and this one can't land after it. Then the
/// caller's account row, so its posts are counted one at a time.
async fn begin_write<'a>(
    state: &'a AppState,
    user: &AuthUser,
) -> AppResult<Transaction<'a, Postgres>> {
    let mut tx = state.db.begin().await?;
    crate::modes::hold_public(&mut tx, OFF, OFF_MESSAGE).await?;
    sqlx::query("SELECT 1 FROM users WHERE id = $1 FOR NO KEY UPDATE")
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// The categories a forum starts with, when it has none. Run as the server
/// goes public. Says whether it added them.
pub async fn seed(tx: &mut Transaction<'_, Postgres>) -> AppResult<bool> {
    sqlx::query("UPDATE server_settings SET forum_seeded = true")
        .execute(&mut **tx)
        .await?;
    let any: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM forum_categories)")
        .fetch_one(&mut **tx)
        .await?;
    if any {
        return Ok(false);
    }
    for (position, (name, description)) in (0..).zip(DEFAULT_CATEGORIES) {
        sqlx::query(
            "INSERT INTO forum_categories (name, description, position) VALUES ($1, $2, $3)",
        )
        .bind(name)
        .bind(description)
        .bind(position)
        .execute(&mut **tx)
        .await?;
    }
    Ok(true)
}

/// [`seed`], unless the forum has had its categories before: run at each
/// start of a public server, so one that went public before the forum
/// existed gets them, and one whose admin deleted them all doesn't get
/// them back.
pub async fn seed_once(tx: &mut Transaction<'_, Postgres>) -> AppResult<bool> {
    let seeded: Option<bool> = sqlx::query_scalar("SELECT forum_seeded FROM server_settings")
        .fetch_optional(&mut **tx)
        .await?;
    if seeded == Some(true) {
        return Ok(false);
    }
    seed(tx).await
}

/// What an account must wait for before it posts again.
#[derive(Debug, PartialEq, Eq)]
enum Wait {
    /// It posted less than `POST_INTERVAL_SECS` ago.
    Interval(u64),
    /// It has posted `POSTS_PER_DAY` times in a day.
    Day(u64),
}

/// The wait before the next post, from how long ago the account's latest
/// post of the last day was written, how many it wrote in that day and how
/// long ago the oldest of them was, in seconds. `None`: it may post.
fn posting_wait(latest_age: Option<i64>, today: i64, oldest_age: Option<i64>) -> Option<Wait> {
    let left = |total: i64, age: i64| u64::try_from(total.saturating_sub(age).max(1)).unwrap_or(1);
    if today >= POSTS_PER_DAY {
        return Some(Wait::Day(left(DAY_SECS, oldest_age.unwrap_or(0))));
    }
    match latest_age {
        Some(age) if age < POST_INTERVAL_SECS => {
            Some(Wait::Interval(left(POST_INTERVAL_SECS, age)))
        }
        _ => None,
    }
}

fn wait_error(wait: Wait) -> AppError {
    match wait {
        Wait::Interval(secs) => AppError::Slow {
            retry_after: secs,
            message: format!(
                "you can post again in {secs} second{}",
                if secs == 1 { "" } else { "s" }
            ),
        },
        Wait::Day(secs) => {
            let hours = secs.div_ceil(3600);
            AppError::Slow {
                retry_after: secs,
                message: format!(
                    "you've posted {POSTS_PER_DAY} times today; you can post again in {}",
                    if hours <= 1 {
                        format!("{} minutes", secs.div_ceil(60).max(1))
                    } else {
                        format!("{hours} hours")
                    }
                ),
            }
        }
    }
}

/// Refuses a post that comes too soon after the account's last, or past its
/// day's posts. Admins aren't held to it. Run in a [`begin_write`].
async fn check_pace(tx: &mut Transaction<'_, Postgres>, user: &AuthUser) -> AppResult<()> {
    if user.role == Role::Admin {
        return Ok(());
    }
    let (latest, today, oldest): (Option<i64>, i64, Option<i64>) = sqlx::query_as(
        "SELECT floor(EXTRACT(EPOCH FROM now() - max(created_at)))::bigint, count(*), \
                floor(EXTRACT(EPOCH FROM now() - min(created_at)))::bigint \
         FROM forum_posts WHERE author_id = $1 AND created_at > now() - interval '1 day'",
    )
    .bind(user.id)
    .fetch_one(&mut **tx)
    .await?;
    match posting_wait(latest, today, oldest) {
        Some(wait) => Err(wait_error(wait)),
        None => Ok(()),
    }
}

fn author(
    state: &AppState,
    name: Option<String>,
    display_name: Option<String>,
    avatar: Option<Vec<u8>>,
) -> Option<UserCard> {
    let avatar = avatar.map(hex::encode);
    name.map(|name| avatars::card(state, name, display_name, avatar.as_deref()))
}

fn count(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

/// Page and size from a query, within bounds.
fn paging(page: Option<u32>, limit: Option<u32>, default: u32) -> (u32, u32, i64) {
    let page = page.unwrap_or(1).clamp(1, MAX_PAGE);
    let limit = limit.unwrap_or(default).clamp(1, MAX_FORUM_LIMIT);
    (page, limit, i64::from(page - 1) * i64::from(limit))
}

fn conflict(code: &'static str, message: &str) -> AppError {
    AppError::Conflict {
        code,
        message: message.into(),
    }
}

fn locked() -> AppError {
    conflict("locked", "this thread is locked")
}

// Categories

#[derive(FromRow)]
struct CategoryRow {
    id: i64,
    name: String,
    description: String,
    position: i32,
    threads: i64,
    last_activity_at: Option<DateTime<Utc>>,
}

impl From<CategoryRow> for ForumCategory {
    fn from(r: CategoryRow) -> Self {
        ForumCategory {
            id: r.id,
            name: r.name,
            description: r.description,
            position: r.position,
            threads: count(r.threads),
            last_activity_at: r.last_activity_at,
        }
    }
}

macro_rules! category_sql {
    ($rest:literal) => {
        concat!(
            "SELECT c.id, c.name, c.description, c.position, \
                 (SELECT count(*) FROM forum_threads t WHERE t.category_id = c.id) AS threads, \
                 (SELECT max(t.last_post_at) FROM forum_threads t WHERE t.category_id = c.id) \
                     AS last_activity_at \
             FROM forum_categories c ",
            $rest
        )
    };
}

async fn category(db: impl sqlx::PgExecutor<'_>, id: i64) -> AppResult<ForumCategory> {
    let row: CategoryRow = sqlx::query_as(category_sql!("WHERE c.id = $1"))
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(row.into())
}

/// `GET /forum`: the categories, in their order.
pub async fn categories(
    State(state): State<AppState>,
    _viewer: Viewer,
) -> AppResult<Json<Vec<ForumCategory>>> {
    let rows: Vec<CategoryRow> =
        sqlx::query_as(category_sql!("ORDER BY c.position, lower(c.name), c.id"))
            .fetch_all(&state.db)
            .await?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

fn name_taken(e: sqlx::Error) -> AppError {
    match AppError::from(e) {
        AppError::Conflict { .. } => conflict("name_taken", "a category has that name already"),
        other => other,
    }
}

/// `POST /forum/categories` (admins).
pub async fn create_category(
    State(state): State<AppState>,
    user: AuthUser,
    Json(req): Json<NewCategory>,
) -> AppResult<Json<ForumCategory>> {
    require_admin_session(&user)?;
    let name = clean_category_name(&req.name)?;
    let description = clean_description(&req.description)?;
    let mut tx = state.db.begin().await?;
    sqlx::query("LOCK TABLE forum_categories IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    let (n, last): (i64, Option<i32>) =
        sqlx::query_as("SELECT count(*), max(position) FROM forum_categories")
            .fetch_one(&mut *tx)
            .await?;
    if usize::try_from(n).unwrap_or(usize::MAX) >= MAX_CATEGORIES {
        return Err(conflict(
            "too_many_categories",
            &format!("a forum has at most {MAX_CATEGORIES} categories"),
        ));
    }
    let position = req
        .position
        .unwrap_or_else(|| last.map_or(0, |p| p.saturating_add(1)));
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO forum_categories (name, description, position) VALUES ($1, $2, $3) \
         RETURNING id",
    )
    .bind(&name)
    .bind(&description)
    .bind(position)
    .fetch_one(&mut *tx)
    .await
    .map_err(name_taken)?;
    tx.commit().await?;
    category(&state.db, id).await.map(Json)
}

/// `PATCH /forum/categories/{id}` (admins).
pub async fn update_category(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<CategoryUpdate>,
) -> AppResult<Json<ForumCategory>> {
    require_admin_session(&user)?;
    let name = req.name.as_deref().map(clean_category_name).transpose()?;
    let description = req
        .description
        .as_deref()
        .map(clean_description)
        .transpose()?;
    let done = sqlx::query(
        "UPDATE forum_categories SET name = COALESCE($2, name), \
             description = COALESCE($3, description), position = COALESCE($4, position) \
         WHERE id = $1",
    )
    .bind(id)
    .bind(name)
    .bind(description)
    .bind(req.position)
    .execute(&state.db)
    .await
    .map_err(name_taken)?;
    if done.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    category(&state.db, id).await.map(Json)
}

/// `DELETE /forum/categories/{id}` (admins): only an empty one.
pub async fn delete_category(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    require_admin_session(&user)?;
    let mut tx = state.db.begin().await?;
    let found: Option<i64> =
        sqlx::query_scalar("SELECT id FROM forum_categories WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    found.ok_or(AppError::NotFound)?;
    let used: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM forum_threads WHERE category_id = $1)")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    if used {
        return Err(conflict(
            "category_not_empty",
            "move or delete its threads first",
        ));
    }
    sqlx::query("DELETE FROM forum_categories WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// Threads

#[derive(FromRow)]
struct ThreadRow {
    id: i64,
    category_id: i64,
    title: String,
    created_at: DateTime<Utc>,
    last_post_at: DateTime<Utc>,
    post_count: i32,
    pinned: bool,
    locked: bool,
    author_id: Option<i64>,
    author_name: Option<String>,
    author_display: Option<String>,
    author_avatar: Option<Vec<u8>>,
}

const THREAD_COLUMNS: &str = "SELECT t.id, t.category_id, t.title, t.created_at, t.last_post_at, \
     t.post_count, t.pinned, t.locked, t.author_id, u.username AS author_name, \
     u.display_name AS author_display, u.avatar_sha AS author_avatar \
     FROM forum_threads t LEFT JOIN users u ON u.id = t.author_id";

fn to_thread(state: &AppState, r: ThreadRow) -> ForumThread {
    ForumThread {
        id: r.id,
        category_id: r.category_id,
        title: r.title,
        author: author(state, r.author_name, r.author_display, r.author_avatar),
        created_at: r.created_at,
        last_activity_at: r.last_post_at,
        posts: u64::try_from(r.post_count).unwrap_or(0),
        pinned: r.pinned,
        locked: r.locked,
    }
}

async fn thread_row(
    tx: &mut Transaction<'_, Postgres>,
    id: i64,
    lock: bool,
) -> AppResult<ThreadRow> {
    let mut qb = QueryBuilder::<Postgres>::new(THREAD_COLUMNS);
    qb.push(" WHERE t.id = ").push_bind(id);
    if lock {
        qb.push(" FOR UPDATE OF t");
    }
    qb.build_query_as::<ThreadRow>()
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AppError::NotFound)
}

async fn thread(state: &AppState, id: i64) -> AppResult<ForumThread> {
    let mut tx = state.db.begin().await?;
    let row = thread_row(&mut tx, id, false).await?;
    tx.commit().await?;
    Ok(to_thread(state, row))
}

#[derive(Deserialize)]
pub struct ThreadsQuery {
    category: Option<i64>,
    #[serde(default)]
    q: String,
    page: Option<u32>,
    limit: Option<u32>,
}

/// `GET /forum/threads?category=&q=&page=&limit=`: a category's threads,
/// pinned first, then by their latest post; or every category's. `q` finds
/// threads whose title or a post has all its words.
pub async fn threads(
    State(state): State<AppState>,
    _viewer: Viewer,
    Query(params): Query<ThreadsQuery>,
) -> AppResult<Json<ThreadPage>> {
    let (page, limit, offset) = paging(params.page, params.limit, DEFAULT_THREADS_LIMIT);
    let q = params.q.trim();
    if q.chars().count() > MAX_QUERY_CHARS {
        return Err(AppError::BadRequest(format!(
            "search for at most {MAX_QUERY_CHARS} characters"
        )));
    }
    if let Some(id) = params.category {
        category(&state.db, id).await?;
    }
    let mut qb = QueryBuilder::<Postgres>::new(THREAD_COLUMNS);
    qb.push(" WHERE TRUE");
    if let Some(id) = params.category {
        qb.push(" AND t.category_id = ").push_bind(id);
    }
    if !q.is_empty() {
        // Matches the GIN indexes of migration 0037.
        qb.push(" AND (to_tsvector('simple', t.title) @@ plainto_tsquery('simple', ")
            .push_bind(q.to_owned())
            .push(
                ") OR t.id IN (SELECT f.thread_id FROM forum_posts f WHERE f.deleted_at IS NULL \
                 AND to_tsvector('simple', f.body) @@ plainto_tsquery('simple', ",
            )
            .push_bind(q.to_owned())
            .push(")))");
    }
    qb.push(" ORDER BY ");
    if params.category.is_some() && q.is_empty() {
        qb.push("t.pinned DESC, ");
    }
    qb.push("t.last_post_at DESC, t.id DESC LIMIT ")
        .push_bind(i64::from(limit) + 1)
        .push(" OFFSET ")
        .push_bind(offset);
    let mut rows = qb
        .build_query_as::<ThreadRow>()
        .fetch_all(&state.db)
        .await?;
    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    Ok(Json(ThreadPage {
        threads: rows.into_iter().map(|r| to_thread(&state, r)).collect(),
        page,
        limit,
        has_more,
    }))
}

/// `POST /forum/threads`: a thread with its first post.
pub async fn create_thread(
    State(state): State<AppState>,
    user: AuthUser,
    Json(req): Json<NewThread>,
) -> AppResult<Json<ForumThread>> {
    let title = clean_title(&req.title)?;
    let body = clean_body(&req.body)?;
    let mut tx = begin_write(&state, &user).await?;
    let found: Option<i64> =
        sqlx::query_scalar("SELECT id FROM forum_categories WHERE id = $1 FOR SHARE")
            .bind(req.category_id)
            .fetch_optional(&mut *tx)
            .await?;
    if found.is_none() {
        return Err(AppError::BadRequest("there is no such category".into()));
    }
    check_pace(&mut tx, &user).await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO forum_threads (category_id, title, author_id, post_count) \
         VALUES ($1, $2, $3, 1) RETURNING id",
    )
    .bind(req.category_id)
    .bind(&title)
    .bind(user.id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO forum_posts (thread_id, author_id, body) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(user.id)
        .bind(&body)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    thread(&state, id).await.map(Json)
}

/// `PATCH /forum/threads/{id}`: the title by its author or an admin;
/// pinned, locked and the category by admins.
pub async fn update_thread(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<ThreadUpdate>,
) -> AppResult<Json<ForumThread>> {
    let admin = user.role == Role::Admin;
    if !admin && (req.pinned.is_some() || req.locked.is_some() || req.category_id.is_some()) {
        return Err(AppError::Denied("only admins pin, lock or move threads"));
    }
    if admin && (req.pinned.is_some() || req.locked.is_some() || req.category_id.is_some()) {
        require_admin_session(&user)?;
    }
    let title = req.title.as_deref().map(clean_title).transpose()?;
    let mut tx = state.db.begin().await?;
    let row = thread_row(&mut tx, id, true).await?;
    if title.is_some() && !admin {
        if row.author_id != Some(user.id) {
            return Err(AppError::Forbidden);
        }
        if row.locked {
            return Err(locked());
        }
    }
    if let Some(category_id) = req.category_id {
        let found: Option<i64> =
            sqlx::query_scalar("SELECT id FROM forum_categories WHERE id = $1 FOR SHARE")
                .bind(category_id)
                .fetch_optional(&mut *tx)
                .await?;
        if found.is_none() {
            return Err(AppError::BadRequest("there is no such category".into()));
        }
    }
    sqlx::query(
        "UPDATE forum_threads SET title = COALESCE($2, title), pinned = COALESCE($3, pinned), \
             locked = COALESCE($4, locked), category_id = COALESCE($5, category_id) \
         WHERE id = $1",
    )
    .bind(id)
    .bind(title)
    .bind(req.pinned)
    .bind(req.locked)
    .bind(req.category_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    thread(&state, id).await.map(Json)
}

/// `DELETE /forum/threads/{id}` (admins): the thread and all its posts.
pub async fn delete_thread(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    require_admin_session(&user)?;
    let done = sqlx::query("DELETE FROM forum_threads WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    if done.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

// Posts

#[derive(FromRow)]
struct PostRow {
    id: i64,
    thread_id: i64,
    body: String,
    created_at: DateTime<Utc>,
    edited_at: Option<DateTime<Utc>>,
    deleted_at: Option<DateTime<Utc>>,
    author_name: Option<String>,
    author_display: Option<String>,
    author_avatar: Option<Vec<u8>>,
}

const POST_COLUMNS: &str = "SELECT f.id, f.thread_id, f.body, f.created_at, f.edited_at, \
     f.deleted_at, u.username AS author_name, u.display_name AS author_display, \
     u.avatar_sha AS author_avatar \
     FROM forum_posts f LEFT JOIN users u ON u.id = f.author_id";

fn to_post(state: &AppState, r: PostRow) -> ForumPost {
    let deleted = r.deleted_at.is_some();
    ForumPost {
        id: r.id,
        thread_id: r.thread_id,
        // A deleted post says nothing, not even who wrote it.
        author: if deleted {
            None
        } else {
            author(state, r.author_name, r.author_display, r.author_avatar)
        },
        body: if deleted { String::new() } else { r.body },
        created_at: r.created_at,
        edited_at: r.edited_at,
        deleted,
    }
}

async fn forum_post(state: &AppState, id: i64) -> AppResult<ForumPost> {
    let mut qb = QueryBuilder::<Postgres>::new(POST_COLUMNS);
    qb.push(" WHERE f.id = ").push_bind(id);
    let row = qb
        .build_query_as::<PostRow>()
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(to_post(state, row))
}

#[derive(Deserialize)]
pub struct PageQuery {
    page: Option<u32>,
    limit: Option<u32>,
}

/// `GET /forum/threads/{id}?page=&limit=`: the thread and a page of its
/// posts, oldest first.
pub async fn read_thread(
    State(state): State<AppState>,
    _viewer: Viewer,
    Path(id): Path<i64>,
    Query(params): Query<PageQuery>,
) -> AppResult<Json<ThreadView>> {
    let (page, limit, offset) = paging(params.page, params.limit, DEFAULT_POSTS_LIMIT);
    let thread = thread(&state, id).await?;
    let mut qb = QueryBuilder::<Postgres>::new(POST_COLUMNS);
    qb.push(" WHERE f.thread_id = ")
        .push_bind(id)
        .push(" ORDER BY f.id LIMIT ")
        .push_bind(i64::from(limit) + 1)
        .push(" OFFSET ")
        .push_bind(offset);
    let mut rows = qb.build_query_as::<PostRow>().fetch_all(&state.db).await?;
    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    Ok(Json(ThreadView {
        thread,
        posts: rows.into_iter().map(|r| to_post(&state, r)).collect(),
        page,
        limit,
        has_more,
    }))
}

/// `POST /forum/threads/{id}/posts`: a reply.
pub async fn reply(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<ForumPostBody>,
) -> AppResult<Json<ForumPost>> {
    let body = clean_body(&req.body)?;
    let mut tx = begin_write(&state, &user).await?;
    let thread = thread_row(&mut tx, id, true).await?;
    if thread.locked && user.role != Role::Admin {
        return Err(locked());
    }
    check_pace(&mut tx, &user).await?;
    let post: i64 = sqlx::query_scalar(
        "INSERT INTO forum_posts (thread_id, author_id, body) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(id)
    .bind(user.id)
    .bind(&body)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE forum_threads SET post_count = post_count + 1, last_post_at = now() WHERE id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    forum_post(&state, post).await.map(Json)
}

/// A post locked for a change: its author, whether it is deleted, and
/// whether its thread is locked.
async fn lock_post(
    tx: &mut Transaction<'_, Postgres>,
    id: i64,
) -> AppResult<(Option<i64>, bool, bool)> {
    sqlx::query_as(
        "SELECT f.author_id, f.deleted_at IS NOT NULL, t.locked \
         FROM forum_posts f JOIN forum_threads t ON t.id = f.thread_id \
         WHERE f.id = $1 FOR UPDATE OF f",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::NotFound)
}

/// `PATCH /forum/posts/{id}`: its author changes it.
pub async fn edit_post(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<ForumPostBody>,
) -> AppResult<Json<ForumPost>> {
    let body = clean_body(&req.body)?;
    let mut tx = state.db.begin().await?;
    let (author_id, deleted, thread_locked) = lock_post(&mut tx, id).await?;
    if author_id != Some(user.id) {
        return Err(AppError::Denied("only its author changes a post"));
    }
    if deleted {
        return Err(conflict("deleted", "this post was deleted"));
    }
    if thread_locked && user.role != Role::Admin {
        return Err(locked());
    }
    sqlx::query("UPDATE forum_posts SET body = $2, edited_at = now() WHERE id = $1 AND body <> $2")
        .bind(id)
        .bind(&body)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    forum_post(&state, id).await.map(Json)
}

/// `DELETE /forum/posts/{id}`: by its author or an admin. The post keeps
/// its place, empty, so the thread still reads in order.
pub async fn delete_post(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    let mut tx = state.db.begin().await?;
    let (author_id, deleted, _) = lock_post(&mut tx, id).await?;
    if !user.can_moderate(author_id) {
        return Err(AppError::Forbidden);
    }
    if !deleted {
        sqlx::query("UPDATE forum_posts SET body = '', deleted_at = now() WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posting_waits() {
        assert_eq!(posting_wait(None, 0, None), None);
        assert_eq!(posting_wait(Some(15), 1, Some(15)), None);
        assert_eq!(posting_wait(Some(3), 1, Some(3)), Some(Wait::Interval(12)));
        assert_eq!(posting_wait(Some(0), 1, Some(0)), Some(Wait::Interval(15)));
        // A clock gone backwards still waits, not forever.
        assert_eq!(posting_wait(Some(-5), 1, None), Some(Wait::Interval(20)));
        // The day's limit counts from the oldest post of the last day.
        assert_eq!(
            posting_wait(Some(600), POSTS_PER_DAY, Some(DAY_SECS - 90)),
            Some(Wait::Day(90))
        );
        assert_eq!(
            posting_wait(Some(600), POSTS_PER_DAY - 1, Some(DAY_SECS - 90)),
            None
        );
        assert_eq!(
            posting_wait(Some(1), POSTS_PER_DAY, Some(DAY_SECS + 9)),
            Some(Wait::Day(1))
        );
        assert_eq!(
            posting_wait(Some(i64::MIN), 1, None),
            Some(Wait::Interval(u64::try_from(i64::MAX).unwrap()))
        );
    }

    #[test]
    fn waits_say_how_long() {
        let message = |wait| wait_error(wait).to_string();
        assert_eq!(message(Wait::Interval(1)), "you can post again in 1 second");
        assert_eq!(
            message(Wait::Interval(12)),
            "you can post again in 12 seconds"
        );
        assert_eq!(
            message(Wait::Day(90)),
            "you've posted 100 times today; you can post again in 2 minutes"
        );
        assert_eq!(
            message(Wait::Day(5 * 3600)),
            "you've posted 100 times today; you can post again in 5 hours"
        );
    }
}

#[cfg(test)]
mod api {
    use axum::http::{Method, StatusCode};
    use serde_json::{Value, json};

    use crate::testing::{self, TestApp};

    async fn post(app: &TestApp, token: Option<&str>, path: &str, body: Value) -> testing::Resp {
        app.call(Method::POST, path, token, Some(body)).await
    }

    async fn patch(app: &TestApp, token: &str, path: &str, body: Value) -> testing::Resp {
        app.call(Method::PATCH, path, Some(token), Some(body)).await
    }

    fn ids(list: &Value) -> Vec<i64> {
        list.as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_i64().unwrap())
            .collect()
    }

    /// Lets the account post again, as if its posts were older.
    async fn age_posts(app: &TestApp, secs: i64) {
        sqlx::query("UPDATE forum_posts SET created_at = created_at - make_interval(secs => $1)")
            .bind(secs as f64)
            .execute(app.db())
            .await
            .unwrap();
    }

    async fn categories(app: &TestApp) -> Vec<String> {
        sqlx::query_scalar("SELECT name FROM forum_categories ORDER BY position")
            .fetch_all(app.db())
            .await
            .unwrap()
    }

    async fn seeded(app: &TestApp) -> bool {
        sqlx::query_scalar("SELECT forum_seeded FROM server_settings")
            .fetch_one(app.db())
            .await
            .unwrap()
    }

    /// A server already public on 0.3.0 had no categories after the
    /// upgrade: the next start gives it the three, once.
    #[tokio::test]
    async fn an_upgraded_public_server_gets_its_categories_once() {
        use crate::modes::settle;
        let Some(app) = testing::app().await else {
            return;
        };
        // Public before the forum: stored so, with nothing seeded.
        crate::settings::save_mode(app.db(), starbooru_core::Mode::Public)
            .await
            .unwrap();
        assert!(categories(&app).await.is_empty());
        assert!(!seeded(&app).await);

        let s = settle(app.db(), None, None).await.unwrap();
        assert!(s.forum_seeded && !s.first);
        assert_eq!(categories(&app).await, ["General", "Requests", "Site"]);
        assert!(seeded(&app).await);
        let s = settle(app.db(), None, None).await.unwrap();
        assert!(!s.forum_seeded);
        assert_eq!(categories(&app).await.len(), 3);

        // An admin who deletes them all on purpose doesn't get them back.
        sqlx::query("DELETE FROM forum_categories")
            .execute(app.db())
            .await
            .unwrap();
        let s = settle(app.db(), None, None).await.unwrap();
        assert!(!s.forum_seeded);
        assert!(categories(&app).await.is_empty());

        // The migration tells the two apart on servers from before the flag:
        // this one had categories, so it counts as seeded.
        let migration = include_str!("../../migrations/0041_forum_seeded.sql");
        sqlx::query("ALTER TABLE server_settings DROP COLUMN forum_seeded")
            .execute(app.db())
            .await
            .unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(migration))
            .execute(app.db())
            .await
            .unwrap();
        assert!(seeded(&app).await);
        settle(app.db(), None, None).await.unwrap();
        assert!(categories(&app).await.is_empty());
        app.finish().await;
    }

    /// A private server isn't seeded at start; going public seeds it.
    #[tokio::test]
    async fn a_private_server_gets_categories_when_it_goes_public() {
        use crate::modes::settle;
        let Some(app) = testing::app().await else {
            return;
        };
        let admin = app.register("admin").await;
        let s = settle(app.db(), None, None).await.unwrap();
        assert!(!s.forum_seeded);
        assert!(categories(&app).await.is_empty() && !seeded(&app).await);
        app.switch(&admin, "public").await;
        assert_eq!(categories(&app).await.len(), 3);
        assert!(seeded(&app).await);
        app.finish().await;
    }

    #[tokio::test]
    async fn a_private_server_has_no_forum() {
        let Some(app) = testing::app().await else {
            return;
        };
        let admin = app.register("admin").await;
        for (method, path) in [
            (Method::GET, "/api/v1/forum"),
            (Method::GET, "/api/v1/forum/threads"),
            (Method::GET, "/api/v1/forum/threads/1"),
            (Method::POST, "/api/v1/forum/threads"),
            (Method::POST, "/api/v1/forum/threads/1/posts"),
            (Method::PATCH, "/api/v1/forum/posts/1"),
            (Method::DELETE, "/api/v1/forum/posts/1"),
            (Method::POST, "/api/v1/forum/categories"),
        ] {
            for token in [Some(admin.as_str()), None] {
                let r = app.call(method.clone(), path, token, Some(json!({}))).await;
                assert_eq!(
                    (r.status, r.json()["code"].clone()),
                    (StatusCode::FORBIDDEN, json!("forum_off")),
                    "{method} {path}"
                );
            }
        }
        app.finish().await;
    }

    #[tokio::test]
    async fn threads_replies_and_moderation() {
        let Some(app) = testing::app().await else {
            return;
        };
        let admin = app.register("admin").await;
        let ann = app.register("ann").await;
        let bob = app.register("bob").await;
        app.switch(&admin, "public").await;

        // The forum starts with three categories, which guests read.
        let cats = app.call(Method::GET, "/api/v1/forum", None, None).await;
        assert_eq!(cats.status, StatusCode::OK);
        let names: Vec<String> = cats
            .json()
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(names, ["General", "Requests", "Site"]);
        let general = cats.json()[0]["id"].as_i64().unwrap();
        let site = cats.json()[2]["id"].as_i64().unwrap();
        assert_eq!(cats.json()[0]["threads"], 0);
        assert!(cats.json()[0]["last_activity_at"].is_null());

        // A thread, with what a title and a body may be.
        let new = |title: &str, body: &str| json!({ "category_id": general, "title": title, "body": body });
        for (body, status) in [
            (new(" ", "hi"), 400),
            (new("t", " \n "), 400),
            (new(&"t".repeat(121), "hi"), 400),
            (new("t", &"b".repeat(10_001)), 400),
            (
                json!({ "category_id": 999_999, "title": "t", "body": "b" }),
                400,
            ),
        ] {
            let r = post(&app, Some(&ann), "/api/v1/forum/threads", body).await;
            assert_eq!(r.status.as_u16(), status, "{:?}", r.json());
        }
        let r = post(&app, None, "/api/v1/forum/threads", new("t", "b")).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        let r = post(
            &app,
            Some(&ann),
            "/api/v1/forum/threads",
            new("  Best   sky tags ", "# Skies\n\nSee >>1 and **this**."),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        let thread = r.json();
        assert_eq!(thread["title"], "Best sky tags");
        assert_eq!(thread["author"]["name"], "ann");
        assert_eq!(thread["posts"], 1);
        let t = thread["id"].as_i64().unwrap();

        // One post every 15 seconds; admins aren't held to it.
        let reply = format!("/api/v1/forum/threads/{t}/posts");
        let r = post(&app, Some(&ann), &reply, json!({ "body": "me again" })).await;
        assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(r.json()["code"], "rate_limited");
        assert!(
            r.json()["message"]
                .as_str()
                .unwrap()
                .starts_with("you can post again in")
        );
        let wait: u64 = r.headers["retry-after"].to_str().unwrap().parse().unwrap();
        assert!((1..=15).contains(&wait), "{wait}");
        let r = post(&app, Some(&admin), &reply, json!({ "body": "Welcome!" })).await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        let r = post(&app, Some(&admin), &reply, json!({ "body": "And again." })).await;
        assert_eq!(r.status, StatusCode::OK);
        age_posts(&app, 20).await;
        let r = post(&app, Some(&bob), &reply, json!({ "body": "Nice >>2" })).await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        let bobs = r.json();
        assert_eq!(bobs["author"]["name"], "bob");
        assert!(bobs["edited_at"].is_null());

        // A hundred a day.
        sqlx::query(
            "INSERT INTO forum_posts (thread_id, author_id, body, created_at) \
             SELECT $1, (SELECT id FROM users WHERE username = 'ann'), 'old', \
                    now() - interval '23 hours' FROM generate_series(1, 100)",
        )
        .bind(t)
        .execute(app.db())
        .await
        .unwrap();
        let r = post(&app, Some(&ann), &reply, json!({ "body": "one more" })).await;
        assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(
            r.json()["message"]
                .as_str()
                .unwrap()
                .contains("100 times today"),
            "{:?}",
            r.json()
        );
        sqlx::query("DELETE FROM forum_posts WHERE body = 'old'")
            .execute(app.db())
            .await
            .unwrap();

        // The thread reads in order, a page at a time, for guests too.
        let path = format!("/api/v1/forum/threads/{t}");
        let view = app
            .call(Method::GET, &format!("{path}?limit=2"), None, None)
            .await;
        assert_eq!(view.status, StatusCode::OK);
        let view = view.json();
        assert_eq!(view["thread"]["posts"], 4);
        assert_eq!(view["posts"][0]["body"], "# Skies\n\nSee >>1 and **this**.");
        assert_eq!(view["posts"][1]["body"], "Welcome!");
        assert_eq!(view["has_more"], true);
        let rest = app
            .call(Method::GET, &format!("{path}?limit=2&page=2"), None, None)
            .await
            .json();
        assert_eq!(rest["posts"][1]["id"], bobs["id"]);
        assert_eq!(rest["has_more"], false);
        let r = app
            .call(Method::GET, "/api/v1/forum/threads/999999", None, None)
            .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);

        // Edit your own; delete yours (an admin, anyone's).
        let mine = format!("/api/v1/forum/posts/{}", bobs["id"]);
        let r = patch(&app, &ann, &mine, json!({ "body": "not mine" })).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN);
        let r = patch(&app, &bob, &mine, json!({ "body": "Nice! >>2" })).await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(r.json()["body"], "Nice! >>2");
        assert!(r.json()["edited_at"].is_string());
        let r = app.call(Method::DELETE, &mine, Some(&ann), None).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN);
        let r = app.call(Method::DELETE, &mine, Some(&bob), None).await;
        assert_eq!(r.status, StatusCode::NO_CONTENT);
        let r = patch(&app, &bob, &mine, json!({ "body": "back" })).await;
        assert_eq!(r.json()["code"], "deleted");
        let first = format!("/api/v1/forum/posts/{}", view["posts"][0]["id"]);
        let r = app.call(Method::DELETE, &first, Some(&admin), None).await;
        assert_eq!(r.status, StatusCode::NO_CONTENT);
        let view = app.call(Method::GET, &path, None, None).await.json();
        assert_eq!(view["thread"]["posts"], 4, "deleted posts keep their place");
        for gone in [&view["posts"][0], &view["posts"][3]] {
            assert_eq!(
                (&gone["deleted"], &gone["body"], &gone["author"]),
                (&json!(true), &json!(""), &Value::Null)
            );
        }
        assert_eq!(view["thread"]["title"], "Best sky tags");

        // A second thread; pinned first, the latest activity next.
        age_posts(&app, 20).await;
        let r = post(
            &app,
            Some(&bob),
            "/api/v1/forum/threads",
            new("Cats wanted", "Any cats with hats?"),
        )
        .await;
        let cats_thread = r.json()["id"].as_i64().unwrap();
        let list = |q: &str| {
            let (app, q) = (&app, q.to_owned());
            async move {
                let r = app
                    .call(
                        Method::GET,
                        &format!("/api/v1/forum/threads?{q}"),
                        None,
                        None,
                    )
                    .await;
                assert_eq!(r.status, StatusCode::OK, "{q}: {:?}", r.json());
                ids(&r.json()["threads"])
            }
        };
        let in_general = format!("category={general}");
        assert_eq!(list(&in_general).await, [cats_thread, t]);
        let r = patch(&app, &bob, &path, json!({ "pinned": true })).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN);
        let r = patch(&app, &admin, &path, json!({ "pinned": true })).await;
        assert_eq!(r.json()["pinned"], true);
        assert_eq!(list(&in_general).await, [t, cats_thread]);
        assert_eq!(
            list("").await,
            [cats_thread, t],
            "pinned only counts in a category"
        );
        // Search: titles and posts, whole words, any case.
        assert_eq!(list("q=CATS").await, [cats_thread]);
        assert_eq!(list("q=hats").await, [cats_thread]);
        assert_eq!(list("q=welcome").await, [t]);
        assert_eq!(list("q=nothing+here").await, Vec::<i64>::new());
        assert_eq!(list(&format!("category={site}")).await, Vec::<i64>::new());
        let r = app
            .call(
                Method::GET,
                "/api/v1/forum/threads?category=999999",
                None,
                None,
            )
            .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);

        // Locked: only admins post, and its author can't rename it.
        let r = patch(&app, &admin, &path, json!({ "locked": true })).await;
        assert_eq!(r.json()["locked"], true);
        let r = post(&app, Some(&bob), &reply, json!({ "body": "late" })).await;
        assert_eq!(
            (r.status, r.json()["code"].clone()),
            (StatusCode::CONFLICT, json!("locked"))
        );
        let r = patch(&app, &ann, &path, json!({ "title": "Renamed" })).await;
        assert_eq!(r.json()["code"], "locked");
        let r = post(&app, Some(&admin), &reply, json!({ "body": "Closing." })).await;
        assert_eq!(r.status, StatusCode::OK);
        // Its author renames an open thread; others can't.
        let cats_path = format!("/api/v1/forum/threads/{cats_thread}");
        let r = patch(&app, &ann, &cats_path, json!({ "title": "Mine now" })).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN);
        let r = patch(&app, &bob, &cats_path, json!({ "title": "Cats in hats" })).await;
        assert_eq!(r.json()["title"], "Cats in hats");
        // Moved by an admin.
        let r = patch(&app, &admin, &cats_path, json!({ "category_id": site })).await;
        assert_eq!(r.json()["category_id"], site);
        assert_eq!(list(&format!("category={site}")).await, [cats_thread]);
        let cats = app.get("/api/v1/forum", &bob).await.json();
        assert_eq!(
            (&cats[0]["threads"], &cats[2]["threads"]),
            (&json!(1), &json!(1))
        );
        assert!(cats[2]["last_activity_at"].is_string());

        // Categories, by admins.
        let r = post(
            &app,
            Some(&bob),
            "/api/v1/forum/categories",
            json!({ "name": "Mine" }),
        )
        .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN);
        let r = post(
            &app,
            Some(&admin),
            "/api/v1/forum/categories",
            json!({ "name": " Art ", "description": "Drawings" }),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        assert_eq!(
            (&r.json()["name"], &r.json()["position"]),
            (&json!("Art"), &json!(3))
        );
        let art = r.json()["id"].as_i64().unwrap();
        let r = post(
            &app,
            Some(&admin),
            "/api/v1/forum/categories",
            json!({ "name": "art" }),
        )
        .await;
        assert_eq!(r.json()["code"], "name_taken");
        let r = patch(
            &app,
            &admin,
            &format!("/api/v1/forum/categories/{art}"),
            json!({ "position": -1, "description": "" }),
        )
        .await;
        assert_eq!(
            (&r.json()["position"], &r.json()["description"]),
            (&json!(-1), &json!(""))
        );
        assert_eq!(
            app.get("/api/v1/forum", &bob).await.json()[0]["name"],
            "Art"
        );
        let r = app
            .call(
                Method::DELETE,
                &format!("/api/v1/forum/categories/{site}"),
                Some(&admin),
                None,
            )
            .await;
        assert_eq!(r.json()["code"], "category_not_empty");
        let r = app
            .call(
                Method::DELETE,
                &format!("/api/v1/forum/categories/{art}"),
                Some(&admin),
                None,
            )
            .await;
        assert_eq!(r.status, StatusCode::NO_CONTENT);

        // An admin deletes a whole thread.
        let r = app.call(Method::DELETE, &cats_path, Some(&bob), None).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN);
        let r = app
            .call(Method::DELETE, &cats_path, Some(&admin), None)
            .await;
        assert_eq!(r.status, StatusCode::NO_CONTENT);
        assert_eq!(
            app.call(Method::GET, &cats_path, None, None).await.status,
            404
        );

        // Profiles count what is left of an account's posts.
        let ann_profile = app.get("/api/v1/users/ann", &bob).await.json();
        assert_eq!(ann_profile["forum_posts"], 0);
        let admin_profile = app.get("/api/v1/users/admin", &bob).await.json();
        assert_eq!(admin_profile["forum_posts"], 3);
        app.finish().await;
    }
}
