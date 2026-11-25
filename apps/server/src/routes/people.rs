//! People: the characters of the collection (tags in the `character`
//! category on live posts, with figures or without), the franchise each
//! belongs to, and a face for each. `GET /people` and `GET /franchises`
//! list them; Explore's `people` and `popular_characters` come from here.
//!
//! A character's franchise is worked out in the background and kept in
//! `tags.franchise_id` (`fill_due`), so lists can group and filter by it.
//! It is, first found: a copyright tag the character implies, its nearest
//! copyright ancestor in the tag tree, the franchise its figures are given
//! most, and the copyright tag on most of its newest posts. The same task
//! keeps a few faces to show for each in `tags.face_figures`, so a list
//! never sorts all of a character's figures.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use sqlx::{PgConnection, Postgres, QueryBuilder, Transaction};
use starbooru_core::people::{
    DEFAULT_FRANCHISE_LIMIT, DEFAULT_PEOPLE_LIMIT, MAX_FRANCHISE_LIMIT, MAX_PEOPLE_LIMIT,
};
use starbooru_core::{
    Franchise, FranchisePage, PeopleOrder, PeoplePage, Person, SearchQuery, TagCategory,
    normalize_tag, split_category,
};
use tracing::{info, warn};

use super::blacklist::{Hidden, hidden_for};
use super::figures::{CHARACTER_SLOT, COSPLAY_SLOT, FRANCHISE_SLOT};
use super::posts::{TagFilter, push_filters};
use crate::error::AppResult;
use crate::guests::Viewer;
use crate::state::AppState;
use crate::storage::{Storage, Variant};

const MAX_PAGE: u32 = 10_000;
/// Characters whose franchise is worked out in one round.
const FILL_BATCH: i64 = 200;
/// Characters looked up in one query of their newest posts.
const POSTS_BATCH: usize = 50;
/// The newest posts of a character whose copyright tags are counted.
const SAMPLE_POSTS: i64 = 50;
/// How often the franchises are looked at without being asked.
const IDLE: Duration = Duration::from_secs(600);
/// Implications and parent chains are followed this far.
const MAX_DEPTH: i32 = 32;
/// Faces kept for a character for each rating, so a reader who hides one
/// still has some.
const FACES_PER_RATING: i64 = 2;

fn character() -> i16 {
    TagCategory::Character.to_db()
}

fn copyright() -> i16 {
    TagCategory::Copyright.to_db()
}

/// The franchise of each of the characters `ids` that has one, by id. See
/// the module's notes for where it comes from.
async fn resolve(conn: &mut PgConnection, ids: &[i64]) -> AppResult<HashMap<i64, i64>> {
    let mut found: HashMap<i64, i64> = HashMap::new();
    if ids.is_empty() {
        return Ok(found);
    }
    let implied: Vec<(i64, i64)> = sqlx::query_as(
        "WITH RECURSIVE i(root, id, depth) AS ( \
             SELECT tag_id, implied_id, 1 FROM tag_implications WHERE tag_id = ANY($1) \
             UNION ALL SELECT i.root, t.implied_id, i.depth + 1 FROM i \
             JOIN tag_implications t ON t.tag_id = i.id WHERE i.depth < $2 \
         ) SELECT DISTINCT ON (i.root) i.root, i.id FROM i \
           JOIN tags f ON f.id = i.id AND f.category = $3 \
           ORDER BY i.root, i.depth, f.post_count DESC, f.id",
    )
    .bind(ids)
    .bind(MAX_DEPTH)
    .bind(copyright())
    .fetch_all(&mut *conn)
    .await?;
    found.extend(implied);

    let left: Vec<i64> = ids
        .iter()
        .filter(|id| !found.contains_key(id))
        .copied()
        .collect();
    let above: Vec<(i64, i64)> = sqlx::query_as(
        "WITH RECURSIVE up(root, id, depth) AS ( \
             SELECT id, parent_id, 1 FROM tags WHERE id = ANY($1) AND parent_id IS NOT NULL \
             UNION ALL SELECT up.root, t.parent_id, up.depth + 1 FROM up \
             JOIN tags t ON t.id = up.id WHERE t.parent_id IS NOT NULL AND up.depth < $2 \
         ) SELECT DISTINCT ON (up.root) up.root, up.id FROM up \
           JOIN tags f ON f.id = up.id AND f.category = $3 ORDER BY up.root, up.depth",
    )
    .bind(&left)
    .bind(MAX_DEPTH)
    .bind(copyright())
    .fetch_all(&mut *conn)
    .await?;
    found.extend(above);

    // A cosplay's franchise is the costume's, not the cosplayer's.
    let left: Vec<i64> = ids
        .iter()
        .filter(|id| !found.contains_key(id))
        .copied()
        .collect();
    let given: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT DISTINCT ON (ch.tag_id) ch.tag_id, fr.tag_id FROM figure_tags ch \
         JOIN figure_tags fr ON fr.figure_id = ch.figure_id AND fr.slot = $3 \
         JOIN figures f ON f.id = ch.figure_id \
         JOIN posts p ON p.id = f.post_id AND p.deleted_at IS NULL \
         WHERE ch.slot = $2 AND ch.tag_id = ANY($1) AND NOT EXISTS ( \
             SELECT 1 FROM figure_tags cp WHERE cp.figure_id = ch.figure_id AND cp.slot = $4) \
         GROUP BY ch.tag_id, fr.tag_id \
         ORDER BY ch.tag_id, count(*) DESC, fr.tag_id",
    )
    .bind(&left)
    .bind(CHARACTER_SLOT)
    .bind(FRANCHISE_SLOT)
    .bind(COSPLAY_SLOT)
    .fetch_all(&mut *conn)
    .await?;
    found.extend(given);

    let left: Vec<i64> = ids
        .iter()
        .filter(|id| !found.contains_key(id))
        .copied()
        .collect();
    for chunk in left.chunks(POSTS_BATCH) {
        // One branch per character, planned with its id, so a rare one reads
        // its few posts through the tag index and a common one walks the
        // newest posts.
        let mut qb = QueryBuilder::<Postgres>::new("SELECT DISTINCT ON (s.c) s.c, u.id FROM (");
        for (i, id) in chunk.iter().enumerate() {
            if i > 0 {
                qb.push(" UNION ALL ");
            }
            qb.push("(SELECT ")
                .push_bind(*id)
                .push("::bigint AS c, p.tag_ids FROM posts p WHERE p.tag_ids @> ARRAY[")
                .push_bind(*id)
                .push("::bigint] AND p.deleted_at IS NULL ORDER BY p.id DESC LIMIT ")
                .push_bind(SAMPLE_POSTS)
                .push(")");
        }
        qb.push(
            ") s CROSS JOIN LATERAL unnest(s.tag_ids) AS u(id) \
             JOIN tags f ON f.id = u.id AND f.category = ",
        )
        .push_bind(copyright())
        .push(
            " GROUP BY s.c, u.id \
             ORDER BY s.c, count(*) DESC, max(f.post_count) DESC, u.id",
        );
        let most: Vec<(i64, i64)> = qb
            .build_query_as()
            .persistent(false)
            .fetch_all(&mut *conn)
            .await?;
        found.extend(most);
    }
    Ok(found)
}

/// The franchise of each character named in `names` (aliases followed),
/// worked out now, by the name as given.
pub(crate) async fn franchises_by_name(
    tx: &mut Transaction<'_, Postgres>,
    names: &[String],
) -> AppResult<HashMap<String, String>> {
    let tags: Vec<(String, i64)> = sqlx::query_as(
        "SELECT n.name, COALESCE(a.tag_id, t.id) FROM unnest($1::text[]) AS n(name) \
         LEFT JOIN tag_aliases a ON a.alias = n.name \
         LEFT JOIN tags t ON t.name = n.name \
         WHERE COALESCE(a.tag_id, t.id) IS NOT NULL",
    )
    .bind(names)
    .fetch_all(&mut **tx)
    .await?;
    let ids: Vec<i64> = tags.iter().map(|(_, id)| *id).collect();
    let found = resolve(tx, &ids).await?;
    let franchise_ids: Vec<i64> = found.values().copied().collect();
    let franchise_names: HashMap<i64, String> =
        sqlx::query_as("SELECT id, name FROM tags WHERE id = ANY($1)")
            .bind(&franchise_ids)
            .fetch_all(&mut **tx)
            .await?
            .into_iter()
            .collect();
    Ok(tags
        .into_iter()
        .filter_map(|(name, id)| {
            let franchise = franchise_names.get(found.get(&id)?)?;
            Some((name, franchise.clone()))
        })
        .collect())
}

/// Works out the franchise of every character in use that has none worked
/// out yet, or not for a day; the most used first. Returns how many it
/// looked at.
pub async fn fill_due(state: &AppState) -> AppResult<u64> {
    let mut done = 0u64;
    loop {
        let ids = claim_due(state).await?;
        if ids.is_empty() {
            return Ok(done);
        }
        let mut conn = state.db.acquire().await?;
        let found = resolve(&mut conn, &ids).await?;
        store(&mut conn, &ids, &found).await?;
        done = done.saturating_add(ids.len() as u64);
    }
}

/// Marks a batch of the characters due as being worked out, and returns
/// them. Anything that has them worked out again meanwhile sets the mark
/// back to NULL (`recheck`, saving figures, a new crop), which `store`
/// then leaves alone, so the change isn't lost to a result from before it.
/// A mark left by a crash is due like any other.
async fn claim_due(state: &AppState) -> AppResult<Vec<i64>> {
    Ok(sqlx::query_scalar(
        "UPDATE tags SET franchise_checked_at = '-infinity' WHERE id IN ( \
             SELECT id FROM tags WHERE category = $1 AND post_count > 0 \
             AND (franchise_checked_at IS NULL \
                  OR franchise_checked_at < now() - interval '1 day') \
             ORDER BY franchise_checked_at NULLS FIRST, post_count DESC, id LIMIT $2) \
         RETURNING id",
    )
    .bind(character())
    .bind(FILL_BATCH)
    .fetch_all(&state.db)
    .await?)
}

/// Keeps what `resolve` found for the claimed characters `ids`, with the
/// faces to show for each, unless one was asked for again since its claim.
async fn store(conn: &mut PgConnection, ids: &[i64], found: &HashMap<i64, i64>) -> AppResult<()> {
    // 0 for none: tag ids start at 1.
    let franchises: Vec<i64> = ids
        .iter()
        .map(|id| found.get(id).copied().unwrap_or(0))
        .collect();
    // The best-liked live posts' faces, the best few of each rating.
    sqlx::query(
        "UPDATE tags t SET franchise_id = NULLIF(u.f, 0), franchise_checked_at = now(), \
         face_figures = ARRAY( \
             SELECT x.id FROM ( \
                 SELECT f.id, p.score, p.id AS post, row_number() OVER ( \
                     PARTITION BY p.rating ORDER BY p.score DESC, p.id DESC) AS n \
                 FROM figure_tags ft JOIN figures f ON f.id = ft.figure_id \
                 JOIN posts p ON p.id = f.post_id \
                 WHERE ft.tag_id = t.id AND ft.slot = $3 AND f.face_sha IS NOT NULL \
                   AND p.deleted_at IS NULL) x \
             WHERE x.n <= $4 ORDER BY x.score DESC, x.post DESC) \
         FROM unnest($1::bigint[], $2::bigint[]) AS u(id, f) \
         WHERE t.id = u.id AND t.franchise_checked_at = '-infinity'",
    )
    .bind(ids)
    .bind(&franchises)
    .bind(CHARACTER_SLOT)
    .bind(FACES_PER_RATING)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Has the franchise and faces of the tags `ids` worked out again soon, with
/// those of every character whose franchise they may decide: the ones that
/// have one of them as theirs, imply one of them, or sit below one in the
/// tree. Their category, tree or implications changed, which has already
/// been saved: a failure here is logged, and the daily check catches up.
pub(super) async fn recheck(state: &AppState, ids: &[i64]) {
    // UNION, not UNION ALL: a tag reached two ways is followed once.
    let cleared = sqlx::query(
        "WITH RECURSIVE implying(id, depth) AS ( \
             SELECT id, 0 FROM unnest($1::bigint[]) AS s(id) \
             UNION SELECT i.tag_id, implying.depth + 1 FROM tag_implications i \
             JOIN implying ON i.implied_id = implying.id WHERE implying.depth < $2 \
         ), below(id, depth) AS ( \
             SELECT id, 0 FROM unnest($1::bigint[]) AS s(id) \
             UNION SELECT t.id, below.depth + 1 FROM tags t \
             JOIN below ON t.parent_id = below.id WHERE below.depth < $2 \
         ) UPDATE tags SET franchise_checked_at = NULL \
         WHERE franchise_checked_at IS NOT NULL AND (franchise_id = ANY($1) \
             OR id IN (SELECT id FROM implying) OR id IN (SELECT id FROM below))",
    )
    .bind(ids)
    .bind(MAX_DEPTH)
    .execute(&state.db)
    .await;
    if let Err(e) = cleared {
        warn!("characters' franchises weren't marked to work out again: {e}");
    }
    state.franchises.notify_one();
}

/// The background task that keeps characters' franchises worked out:
/// at start, when woken, and every few minutes.
pub async fn run_franchises(state: AppState) {
    loop {
        match fill_due(&state).await {
            Ok(n) if n > 100 => info!("worked out the franchise of {n} characters"),
            Ok(_) => {}
            Err(e) => warn!("characters' franchises: {e}"),
        }
        let _ = tokio::time::timeout(IDLE, state.franchises.notified()).await;
    }
}

/// What a reader's blacklist hides, as the lists here need it.
struct Unseen {
    tags: Vec<i64>,
    ratings: Vec<i16>,
}

impl Unseen {
    fn of(hidden: &Hidden) -> Self {
        Self {
            tags: hidden.tags_except(&HashSet::new()),
            ratings: hidden.ratings_except(&[]),
        }
    }

    /// ` AND` the post `p` is live and not hidden.
    fn push_visible(&self, qb: &mut QueryBuilder<Postgres>) {
        qb.push(" AND p.deleted_at IS NULL AND NOT (p.tag_ids && ")
            .push_bind(self.tags.clone())
            .push(") AND NOT (p.rating = ANY(")
            .push_bind(self.ratings.clone())
            .push("))");
    }
}

/// A character in a list, before its picture is found.
#[derive(sqlx::FromRow)]
struct PersonRow {
    id: i64,
    name: String,
    posts: i32,
    franchise: Option<String>,
    checked: bool,
}

/// Which people a list holds by franchise.
enum ByFranchise {
    Any,
    /// The people without one.
    Without,
    Is(i64),
}

fn like_contains(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('%');
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// A page of the characters in use, without what `unseen` hides: the
/// character, or the franchise it belongs to.
async fn people_rows(
    state: &AppState,
    unseen: &Unseen,
    name: Option<&str>,
    franchise: ByFranchise,
    order: PeopleOrder,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<PersonRow>> {
    let mut qb = QueryBuilder::<Postgres>::new(
        "SELECT c.id, c.name, c.post_count AS posts, f.name AS franchise, \
         c.franchise_checked_at IS NOT NULL AS checked \
         FROM tags c LEFT JOIN tags f ON f.id = c.franchise_id \
         WHERE c.post_count > 0 AND c.category = ",
    );
    qb.push_bind(character());
    if !unseen.tags.is_empty() {
        qb.push(" AND NOT (c.id = ANY(")
            .push_bind(unseen.tags.clone())
            .push(")) AND NOT (COALESCE(c.franchise_id, 0) = ANY(")
            .push_bind(unseen.tags.clone())
            .push("))");
    }
    if let Some(name) = name {
        qb.push(" AND c.name LIKE ")
            .push_bind(like_contains(name))
            .push(" ESCAPE '\\'");
    }
    match franchise {
        ByFranchise::Any => {}
        ByFranchise::Without => {
            qb.push(" AND c.franchise_id IS NULL");
        }
        ByFranchise::Is(id) => {
            qb.push(" AND c.franchise_id = ").push_bind(id);
        }
    }
    qb.push(match order {
        PeopleOrder::Posts => " ORDER BY c.post_count DESC, c.name",
        PeopleOrder::Name => " ORDER BY c.name",
        PeopleOrder::Franchise => {
            " ORDER BY c.franchise_id IS NULL, \
             sum(c.post_count) OVER (PARTITION BY c.franchise_id) DESC, f.name, \
             c.post_count DESC, c.name"
        }
    });
    qb.push(" LIMIT ")
        .push_bind(limit)
        .push(" OFFSET ")
        .push_bind(offset);
    Ok(qb.build_query_as().fetch_all(&state.db).await?)
}

/// What shows a person: a face crop, or else a post's thumbnail.
enum Picture {
    Face(String),
    Thumb { sha: String, ext: String },
}

/// A picture for each of the characters `ids`, from posts the reader may
/// see: their face (`faces`), or else the thumbnail of their newest such
/// post.
async fn pictures(
    state: &AppState,
    ids: &[i64],
    unseen: &Unseen,
) -> AppResult<HashMap<i64, Picture>> {
    let mut out = faces(state, ids, unseen).await?;
    let left: Vec<i64> = ids
        .iter()
        .filter(|id| !out.contains_key(id))
        .copied()
        .collect();
    for chunk in left.chunks(POSTS_BATCH) {
        // One branch per character, planned with its id (see `resolve`).
        let mut qb = QueryBuilder::<Postgres>::new("");
        for (i, id) in chunk.iter().enumerate() {
            if i > 0 {
                qb.push(" UNION ALL ");
            }
            qb.push("(SELECT ")
                .push_bind(*id)
                .push("::bigint, encode(p.sha256, 'hex'), p.ext FROM posts p WHERE p.tag_ids @> ARRAY[")
                .push_bind(*id)
                .push("::bigint] AND p.has_thumb");
            unseen.push_visible(&mut qb);
            qb.push(" ORDER BY p.id DESC LIMIT 1)");
        }
        let thumbs: Vec<(i64, String, String)> = qb
            .build_query_as()
            .persistent(false)
            .fetch_all(&state.db)
            .await?;
        out.extend(
            thumbs
                .into_iter()
                .map(|(id, sha, ext)| (id, Picture::Thumb { sha, ext })),
        );
    }
    Ok(out)
}

/// The face of each of the characters `ids` that has one kept on a post the
/// reader may see: the first of its `face_figures` that is, and that still
/// names the character.
async fn faces(state: &AppState, ids: &[i64], unseen: &Unseen) -> AppResult<HashMap<i64, Picture>> {
    let mut out = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let mut qb = QueryBuilder::<Postgres>::new(
        "SELECT c.id, x.face FROM tags c CROSS JOIN LATERAL ( \
         SELECT encode(f.face_sha, 'hex') AS face \
         FROM unnest(c.face_figures) WITH ORDINALITY AS k(id, n) \
         JOIN figures f ON f.id = k.id JOIN posts p ON p.id = f.post_id \
         WHERE f.face_sha IS NOT NULL AND EXISTS (SELECT 1 FROM figure_tags ft \
             WHERE ft.figure_id = f.id AND ft.tag_id = c.id AND ft.slot = ",
    );
    qb.push_bind(CHARACTER_SLOT).push(")");
    unseen.push_visible(&mut qb);
    qb.push(" ORDER BY k.n LIMIT 1) x WHERE c.id = ANY(")
        .push_bind(ids.to_vec())
        .push(")");
    let found: Vec<(i64, String)> = qb.build_query_as().fetch_all(&state.db).await?;
    out.extend(found.into_iter().map(|(id, sha)| (id, Picture::Face(sha))));
    Ok(out)
}

fn person(
    state: &AppState,
    name: String,
    posts: i64,
    franchise: Option<String>,
    picture: Option<Picture>,
) -> Person {
    let (face_url, thumb_url) = match picture {
        Some(Picture::Face(sha)) => (Some(crate::faces::url(state, &sha)), None),
        Some(Picture::Thumb { sha, ext }) => {
            let file = Storage::file_name(Variant::Thumb, &sha, &ext);
            (
                None,
                Some(
                    state
                        .signer
                        .url(Variant::Thumb, &file, Utc::now().timestamp()),
                ),
            )
        }
        None => (None, None),
    };
    Person {
        name,
        posts,
        franchise,
        face_url,
        thumb_url,
        votes: None,
    }
}

/// The rows as people, with their pictures. Asks for the franchises that
/// aren't worked out yet.
async fn with_pictures(
    state: &AppState,
    rows: Vec<PersonRow>,
    unseen: &Unseen,
) -> AppResult<Vec<Person>> {
    if rows.iter().any(|r| !r.checked) {
        state.franchises.notify_one();
    }
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    let mut found = pictures(state, &ids, unseen).await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let picture = found.remove(&r.id);
            person(state, r.name, i64::from(r.posts), r.franchise, picture)
        })
        .collect())
}

/// Explore's `people` without a search: the characters on the most live
/// posts, less what the reader hides.
pub(super) async fn top(state: &AppState, hidden: &Hidden, limit: i64) -> AppResult<Vec<Person>> {
    let unseen = Unseen::of(hidden);
    let rows = people_rows(
        state,
        &unseen,
        None,
        ByFranchise::Any,
        PeopleOrder::Posts,
        limit,
        0,
    )
    .await?;
    with_pictures(state, rows, &unseen).await
}

/// One character found among some posts: id, name, how many of them,
/// franchise, a thumbnail among them, their votes.
type FoundRow = (
    i64,
    String,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
);

/// The people found, each with their face, as the lists show it, or else a
/// thumbnail from the posts looked at.
async fn found_people(
    state: &AppState,
    rows: Vec<FoundRow>,
    hidden: &Hidden,
) -> AppResult<Vec<Person>> {
    let ids: Vec<i64> = rows.iter().map(|r| r.0).collect();
    let mut found = faces(state, &ids, &Unseen::of(hidden)).await?;
    Ok(rows
        .into_iter()
        .map(
            |(id, name, posts, franchise, thumb_sha, thumb_ext, votes)| {
                let picture = match (found.remove(&id), thumb_sha, thumb_ext) {
                    (Some(face), _, _) => Some(face),
                    (None, Some(sha), Some(ext)) => Some(Picture::Thumb { sha, ext }),
                    _ => None,
                };
                Person {
                    votes,
                    ..person(state, name, posts, franchise, picture)
                }
            },
        )
        .collect())
}

/// ` WHERE` a character, not one the search asks for itself, and not one
/// the reader's blacklist hides (by name or by franchise) unless the
/// search names it, as the lists without a search do.
fn push_characters(qb: &mut QueryBuilder<Postgres>, filter: &TagFilter) {
    qb.push(
        " FROM m CROSS JOIN LATERAL unnest(m.tag_ids) AS u(id) \
         JOIN tags t ON t.id = u.id AND t.category = ",
    )
    .push_bind(character());
    qb.push(" WHERE NOT (u.id = ANY(")
        .push_bind(filter.include.clone())
        .push("))");
    if !filter.hidden.is_empty() {
        qb.push(" AND NOT (u.id = ANY(")
            .push_bind(filter.hidden.clone())
            .push(")) AND NOT (COALESCE(t.franchise_id, 0) = ANY(")
            .push_bind(filter.hidden.clone())
            .push("))");
    }
}

/// The characters `c` found, with their franchise and thumbnail post.
fn push_found(qb: &mut QueryBuilder<Postgres>) {
    qb.push(
        " SELECT c.id, c.name, c.n, fr.name, encode(tp.sha256, 'hex'), tp.ext, c.heat \
         FROM c JOIN tags ct ON ct.id = c.id LEFT JOIN tags fr ON fr.id = ct.franchise_id \
         LEFT JOIN posts tp ON tp.id = c.thumb ORDER BY c.heat DESC NULLS LAST, c.n DESC, c.name",
    );
}

/// Explore's `people` for a search: the characters on the most of the
/// newest `scan` posts it finds (the reader's blacklist applied), without
/// the tags it asks for.
pub(super) async fn among(
    state: &AppState,
    query: &SearchQuery,
    filter: &TagFilter,
    hidden: &Hidden,
    scan: i64,
    limit: i64,
) -> AppResult<Vec<Person>> {
    let mut qb = QueryBuilder::<Postgres>::new(
        "WITH m AS MATERIALIZED (SELECT p.id, p.tag_ids, p.has_thumb FROM posts p",
    );
    push_filters(&mut qb, query, filter);
    qb.push(" ORDER BY p.id DESC LIMIT ").push_bind(scan);
    qb.push(
        "), c AS (SELECT u.id, t.name, count(*) AS n, NULL::bigint AS heat, \
         max(m.id) FILTER (WHERE m.has_thumb) AS thumb",
    );
    push_characters(&mut qb, filter);
    qb.push(" GROUP BY u.id, t.name ORDER BY n DESC, t.name LIMIT ")
        .push_bind(limit)
        .push(")");
    push_found(&mut qb);
    let rows: Vec<FoundRow> = qb.build_query_as().fetch_all(&state.db).await?;
    found_people(state, rows, hidden).await
}

/// Explore's `popular_characters`: the characters on the `scan` posts
/// with the most net upvotes since `since` that the search finds, by the
/// upvotes their posts got.
pub(super) async fn popular(
    state: &AppState,
    query: &SearchQuery,
    filter: &TagFilter,
    hidden: &Hidden,
    since: DateTime<Utc>,
    scan: i64,
    limit: i64,
) -> AppResult<Vec<Person>> {
    // The votes sorted first, so the join reads posts in that order and stops
    // once it has enough.
    let mut qb = QueryBuilder::<Postgres>::new(
        "WITH m AS MATERIALIZED (SELECT p.id, p.tag_ids, p.has_thumb, h.heat FROM \
         (SELECT post_id, sum(value) AS heat FROM post_votes WHERE voted_at >= ",
    );
    qb.push_bind(since).push(
        " GROUP BY post_id HAVING sum(value) > 0 ORDER BY heat DESC, post_id DESC) h \
         JOIN posts p ON p.id = h.post_id",
    );
    push_filters(&mut qb, query, filter);
    qb.push(" ORDER BY h.heat DESC, h.post_id DESC LIMIT ")
        .push_bind(scan);
    qb.push(
        "), c AS (SELECT u.id, t.name, count(*) AS n, sum(m.heat)::bigint AS heat, \
         (array_agg(m.id ORDER BY m.heat DESC, m.id DESC) FILTER (WHERE m.has_thumb))[1] AS thumb",
    );
    push_characters(&mut qb, filter);
    qb.push(" GROUP BY u.id, t.name ORDER BY heat DESC, n DESC, t.name LIMIT ")
        .push_bind(limit)
        .push(")");
    push_found(&mut qb);
    let rows: Vec<FoundRow> = qb.build_query_as().fetch_all(&state.db).await?;
    found_people(state, rows, hidden).await
}

#[derive(Deserialize)]
pub struct PeopleParams {
    #[serde(default)]
    q: String,
    franchise: Option<String>,
    #[serde(default)]
    order: PeopleOrder,
    page: Option<u32>,
    limit: Option<u32>,
    /// A guest's own blacklist.
    exclude: Option<String>,
}

/// A name as typed, as the tag it would be; `None` when it can't be one.
fn tag_name(raw: &str) -> Option<String> {
    let name = normalize_tag(raw).ok()?;
    Some(split_category(&name).1.to_owned())
}

/// The id of the tag `name` means, through an alias.
async fn tag_id(state: &AppState, name: &str) -> AppResult<Option<i64>> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE((SELECT tag_id FROM tag_aliases WHERE alias = $1), \
                         (SELECT id FROM tags WHERE name = $1))",
    )
    .bind(name)
    .fetch_one(&state.db)
    .await?)
}

/// `GET /people?q=&franchise=&order=&page=&limit=&exclude=`.
pub async fn list(
    State(state): State<AppState>,
    viewer: Viewer,
    Query(params): Query<PeopleParams>,
) -> AppResult<Json<PeoplePage>> {
    let page = params.page.unwrap_or(1).clamp(1, MAX_PAGE);
    let limit = params
        .limit
        .unwrap_or(DEFAULT_PEOPLE_LIMIT)
        .clamp(1, MAX_PEOPLE_LIMIT);
    let empty = PeoplePage {
        page,
        limit,
        ..PeoplePage::default()
    };
    let name = match params.q.trim() {
        "" => None,
        raw => match tag_name(raw) {
            Some(name) => Some(name),
            None => return Ok(Json(empty)),
        },
    };
    let franchise = match params.franchise.as_deref().map(str::trim) {
        None => ByFranchise::Any,
        Some("") => ByFranchise::Without,
        Some(raw) => match tag_name(raw) {
            Some(name) => match tag_id(&state, &name).await? {
                Some(id) => ByFranchise::Is(id),
                None => return Ok(Json(empty)),
            },
            None => return Ok(Json(empty)),
        },
    };
    let hidden = hidden_for(&state, &viewer, params.exclude.as_deref()).await?;
    let unseen = Unseen::of(&hidden);
    let offset = i64::from(page - 1) * i64::from(limit);
    let mut rows = people_rows(
        &state,
        &unseen,
        name.as_deref(),
        franchise,
        params.order,
        i64::from(limit) + 1,
        offset,
    )
    .await?;
    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    Ok(Json(PeoplePage {
        people: with_pictures(&state, rows, &unseen).await?,
        page,
        limit,
        has_more,
    }))
}

#[derive(Deserialize)]
pub struct FranchiseParams {
    #[serde(default)]
    q: String,
    page: Option<u32>,
    limit: Option<u32>,
    /// A guest's own blacklist.
    exclude: Option<String>,
}

/// `GET /franchises?q=&page=&limit=&exclude=`: the franchises with people,
/// the most people first.
pub async fn franchises(
    State(state): State<AppState>,
    viewer: Viewer,
    Query(params): Query<FranchiseParams>,
) -> AppResult<Json<FranchisePage>> {
    let page = params.page.unwrap_or(1).clamp(1, MAX_PAGE);
    let limit = params
        .limit
        .unwrap_or(DEFAULT_FRANCHISE_LIMIT)
        .clamp(1, MAX_FRANCHISE_LIMIT);
    let name = match params.q.trim() {
        "" => None,
        raw => match tag_name(raw) {
            Some(name) => Some(name),
            None => {
                return Ok(Json(FranchisePage {
                    page,
                    limit,
                    ..FranchisePage::default()
                }));
            }
        },
    };
    let hidden = hidden_for(&state, &viewer, params.exclude.as_deref()).await?;
    let unseen = Unseen::of(&hidden);
    let people = |qb: &mut QueryBuilder<Postgres>| {
        qb.push(" WHERE c.post_count > 0 AND c.category = ")
            .push_bind(character());
        if !unseen.tags.is_empty() {
            qb.push(" AND NOT (c.id = ANY(")
                .push_bind(unseen.tags.clone())
                .push(")) AND NOT (COALESCE(c.franchise_id, 0) = ANY(")
                .push_bind(unseen.tags.clone())
                .push("))");
        }
    };
    let mut qb = QueryBuilder::<Postgres>::new(
        "SELECT f.name, count(*) AS people, f.post_count::bigint AS posts \
         FROM tags c JOIN tags f ON f.id = c.franchise_id",
    );
    people(&mut qb);
    if let Some(name) = &name {
        qb.push(" AND f.name LIKE ")
            .push_bind(like_contains(name))
            .push(" ESCAPE '\\'");
    }
    qb.push(" GROUP BY f.id ORDER BY people DESC, posts DESC, f.name LIMIT ")
        .push_bind(i64::from(limit) + 1)
        .push(" OFFSET ")
        .push_bind(i64::from(page - 1) * i64::from(limit));
    let mut rows: Vec<(String, i64, i64)> = qb.build_query_as().fetch_all(&state.db).await?;
    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);

    let mut qb = QueryBuilder::<Postgres>::new("SELECT count(*) FROM tags c");
    people(&mut qb);
    qb.push(" AND c.franchise_id IS NULL");
    let without: i64 = qb.build_query_scalar().fetch_one(&state.db).await?;
    Ok(Json(FranchisePage {
        franchises: rows
            .into_iter()
            .map(|(name, people, posts)| Franchise {
                name,
                people,
                posts,
            })
            .collect(),
        page,
        limit,
        has_more,
        without,
    }))
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use serde_json::{Value, json};
    use starbooru_core::Mode;

    use crate::testing::{self, TestApp};

    fn names(list: &Value) -> Vec<String> {
        list.as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap().to_owned())
            .collect()
    }

    /// The first person `/people` finds for `q`.
    async fn person(app: &TestApp, t: &str, q: &str) -> Value {
        let r = app.get(&format!("/api/v1/people?q={q}"), t).await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        r.json()["people"][0].clone()
    }

    async fn patch_tag(app: &TestApp, t: &str, name: &str, body: Value) {
        let r = app
            .call(
                Method::PATCH,
                &format!("/api/v1/tags/{name}"),
                Some(t),
                Some(body),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
    }

    /// Saber on three posts (one with her face kept), Rin on one, both of
    /// Fate; Miku on one, of no franchise; a figure with no name on another.
    /// Returns the posts: saber, saber, saber with a face, rin, miku, nameless.
    async fn seed(app: &TestApp, t: &str) -> Vec<i64> {
        let a = app.upload_png(t, 1, "saber fate_series sky").await;
        let b = app.upload_png(t, 2, "saber").await;
        let r = app
            .upload(t, &testing::png(200, 100, 3), &[("tags", "saber")])
            .await;
        let c = r.json()["id"].as_i64().unwrap();
        let d = app.upload_png(t, 4, "rin fate_series").await;
        let e = app.upload_png(t, 5, "miku").await;
        let f = app.upload_png(t, 6, "sky").await;
        for name in ["saber", "rin", "miku"] {
            patch_tag(app, t, name, json!({"category": "character"})).await;
        }
        patch_tag(app, t, "fate_series", json!({"category": "copyright"})).await;
        let put = |id: i64, figures: Value| async move {
            let r = app
                .call(
                    Method::PUT,
                    &format!("/api/v1/posts/{id}/figures"),
                    Some(t),
                    Some(json!({ "figures": figures })),
                )
                .await;
            assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
            r.json()
        };
        let post = put(
            c,
            json!([{"kind": "girl", "character": "saber",
                    "face": {"x": 100, "y": 200, "w": 200, "h": 300}}]),
        )
        .await;
        // Filled in from the copyright tag her other posts have.
        assert_eq!(post["figures"][0]["franchise"], "fate_series");
        put(
            f,
            json!([{"kind": "girl", "tags": {"hair": ["long_hair"]}}]),
        )
        .await;
        assert_eq!(crate::faces::make_due(&app.state).await.unwrap(), 1);
        assert_eq!(super::fill_due(&app.state).await.unwrap(), 3);
        assert_eq!(super::fill_due(&app.state).await.unwrap(), 0);
        vec![a, b, c, d, e, f]
    }

    #[tokio::test]
    async fn lists_people_and_franchises() {
        let Some(app) = testing::app().await else {
            return;
        };
        let t = app.register("admin").await;
        let posts = seed(&app, &t).await;

        let r = app.get("/api/v1/people", &t).await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        let page = r.json();
        // Most posts first; a figure without a name is nobody here.
        assert_eq!(names(&page["people"]), ["saber", "miku", "rin"]);
        assert_eq!(page["has_more"], false);
        let saber = &page["people"][0];
        assert_eq!(saber["posts"], 3);
        assert_eq!(saber["franchise"], "fate_series");
        assert!(
            saber["face_url"]
                .as_str()
                .unwrap()
                .starts_with("/media/face/")
        );
        assert_eq!(saber["thumb_url"], Value::Null);
        // Without a face, a thumbnail of one of their posts.
        let miku = &page["people"][1];
        assert_eq!(miku["franchise"], Value::Null);
        assert_eq!(miku["face_url"], Value::Null);
        assert!(
            miku["thumb_url"]
                .as_str()
                .unwrap()
                .starts_with("/media/thumb/")
        );
        let r = app
            .call(Method::GET, miku["thumb_url"].as_str().unwrap(), None, None)
            .await;
        assert_eq!(r.status, StatusCode::OK);

        let list = |q: &'static str| {
            let (app, t) = (&app, &t);
            async move {
                let r = app.get(&format!("/api/v1/people?{q}"), t).await;
                assert_eq!(r.status, StatusCode::OK, "{q}: {:?}", r.json());
                r.json()
            }
        };
        assert_eq!(
            names(&list("order=name").await["people"]),
            ["miku", "rin", "saber"]
        );
        // By franchise: Fate's people first, the most posts first among them.
        assert_eq!(
            names(&list("order=franchise").await["people"]),
            ["saber", "rin", "miku"]
        );
        assert_eq!(
            names(&list("franchise=Fate+Series").await["people"]),
            ["saber", "rin"]
        );
        assert_eq!(names(&list("franchise=").await["people"]), ["miku"]);
        assert_eq!(
            names(&list("franchise=nowhere").await["people"]),
            Vec::<String>::new()
        );
        assert_eq!(names(&list("q=SAB").await["people"]), ["saber"]);
        assert_eq!(names(&list("q=%25").await["people"]), Vec::<String>::new());
        let first = list("limit=2").await;
        assert_eq!(
            (names(&first["people"]).len(), &first["has_more"]),
            (2, &json!(true))
        );
        let second = list("limit=2&page=2").await;
        assert_eq!(names(&second["people"]), ["rin"]);
        assert_eq!(second["has_more"], false);
        let r = app.get("/api/v1/people?order=height", &t).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);

        let r = app.get("/api/v1/franchises", &t).await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        let page = r.json();
        assert_eq!(
            page["franchises"],
            json!([{"name": "fate_series", "people": 2, "posts": 3}])
        );
        assert_eq!(page["without"], 1);
        let r = app.get("/api/v1/franchises?q=zzz", &t).await.json();
        assert_eq!(r["franchises"], json!([]));

        // A post in the bin counts for nobody.
        let gone = app.upload_png(&t, 9, "miku").await;
        let r = app.get("/api/v1/people", &t).await.json();
        assert_eq!(r["people"][1]["posts"], 2);
        app.call(
            Method::DELETE,
            &format!("/api/v1/posts/{gone}"),
            Some(&t),
            None,
        )
        .await;
        let r = app.get("/api/v1/people", &t).await.json();
        assert_eq!(r["people"][1]["posts"], 1);

        // The franchise follows the collection's rules once they change.
        let r = app
            .call(
                Method::POST,
                "/api/v1/tags/miku/implications",
                Some(&t),
                Some(json!({"implies": "vocaloid"})),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK);
        patch_tag(&app, &t, "vocaloid", json!({"category": "copyright"})).await;
        assert_eq!(super::fill_due(&app.state).await.unwrap(), 1);
        let r = app.get("/api/v1/people?q=miku", &t).await.json();
        assert_eq!(r["people"][0]["franchise"], "vocaloid");

        // With the post of her face in the bin, Saber shows a thumbnail.
        let r = app
            .call(
                Method::DELETE,
                &format!("/api/v1/posts/{}", posts[2]),
                Some(&t),
                None,
            )
            .await;
        assert!(r.status.is_success());
        let r = app.get("/api/v1/people?q=saber", &t).await.json();
        assert_eq!(r["people"][0]["face_url"], Value::Null);
        assert!(r["people"][0]["thumb_url"].is_string());
        app.finish().await;
    }

    /// Regression: a character taken off a post kept showing the face of
    /// the figure that no longer named them, for up to a day.
    #[tokio::test]
    async fn a_face_goes_with_the_name_taken_off() {
        let Some(app) = testing::app().await else {
            return;
        };
        let t = app.register("admin").await;
        let posts = seed(&app, &t).await;
        assert!(person(&app, &t, "saber").await["face_url"].is_string());
        let r = app
            .call(
                Method::PATCH,
                &format!("/api/v1/posts/{}", posts[2]),
                Some(&t),
                Some(json!({"tags": ["sky"]})),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        assert_eq!(r.json()["figures"][0].get("character"), None);
        let saber = person(&app, &t, "saber").await;
        assert_eq!(saber["posts"], 2);
        assert_eq!(saber["face_url"], Value::Null);
        assert!(saber["thumb_url"].is_string());
        // And it is worked out again soon.
        let checked: bool = sqlx::query_scalar(
            "SELECT franchise_checked_at IS NOT NULL FROM tags WHERE name = 'saber'",
        )
        .fetch_one(app.db())
        .await
        .unwrap();
        assert!(!checked);
        assert_eq!(super::fill_due(&app.state).await.unwrap(), 1);
        assert_eq!(person(&app, &t, "saber").await["face_url"], Value::Null);
        app.finish().await;
    }

    /// Regression: a change to a tag a character implies, or to one above
    /// it in the tree, left the character's franchise as it was for a day.
    #[tokio::test]
    async fn a_franchise_follows_changes_further_away() {
        let Some(app) = testing::app().await else {
            return;
        };
        let t = app.register("admin").await;
        app.upload_png(&t, 1, "miku rin").await;
        for name in ["miku", "rin"] {
            patch_tag(&app, &t, name, json!({"category": "character"})).await;
        }
        for (tag, implies) in [("miku", "vocal_synth"), ("vocal_synth", "crypton")] {
            let r = app
                .call(
                    Method::POST,
                    &format!("/api/v1/tags/{tag}/implications"),
                    Some(&t),
                    Some(json!({"implies": implies})),
                )
                .await;
            assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        }
        patch_tag(&app, &t, "rin", json!({"parent": "tohsaka_family"})).await;
        patch_tag(&app, &t, "fate_series", json!({"category": "copyright"})).await;
        assert_eq!(super::fill_due(&app.state).await.unwrap(), 2);
        assert_eq!(person(&app, &t, "miku").await["franchise"], Value::Null);

        // Two implications away.
        patch_tag(&app, &t, "crypton", json!({"category": "copyright"})).await;
        assert_eq!(super::fill_due(&app.state).await.unwrap(), 1);
        assert_eq!(person(&app, &t, "miku").await["franchise"], "crypton");
        // A tag above her moved under a copyright.
        patch_tag(&app, &t, "tohsaka_family", json!({"parent": "fate_series"})).await;
        assert_eq!(super::fill_due(&app.state).await.unwrap(), 1);
        assert_eq!(person(&app, &t, "rin").await["franchise"], "fate_series");
        app.finish().await;
    }

    /// Regression: with a search, Explore still showed the people of a
    /// blacklisted franchise.
    #[tokio::test]
    async fn a_search_hides_the_people_the_blacklist_does() {
        let Some(app) = testing::app().await else {
            return;
        };
        let t = app.register("admin").await;
        seed(&app, &t).await;
        let r = app
            .call(
                Method::PUT,
                "/api/v1/auth/blacklist",
                Some(&t),
                Some(json!({"entries": ["fate_series"]})),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK);
        // Saber's second post has no `fate_series`, but she is of it.
        let e = app.get("/api/v1/explore?q=-miku", &t).await.json();
        assert_eq!(names(&e["people"]), Vec::<String>::new());
        let r = app
            .call(
                Method::PUT,
                "/api/v1/auth/blacklist",
                Some(&t),
                Some(json!({"entries": ["saber"]})),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK);
        let e = app.get("/api/v1/explore?q=fate_series", &t).await.json();
        assert_eq!(names(&e["people"]), ["rin"]);
        // What the search names stays, as its posts do.
        let r = app
            .call(
                Method::PUT,
                "/api/v1/auth/blacklist",
                Some(&t),
                Some(json!({"entries": ["fate_series"]})),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK);
        let e = app.get("/api/v1/explore?q=fate_series", &t).await.json();
        assert_eq!(names(&e["people"]), ["saber", "rin"]);
        app.finish().await;
    }

    /// Regression: a change that came while a round was working a
    /// character out lost to that round's older result, for a day.
    #[tokio::test]
    async fn a_change_during_a_round_is_not_lost() {
        let Some(app) = testing::app().await else {
            return;
        };
        let t = app.register("admin").await;
        app.upload_png(&t, 1, "miku").await;
        patch_tag(&app, &t, "miku", json!({"category": "character"})).await;
        patch_tag(&app, &t, "vocaloid", json!({"category": "copyright"})).await;
        assert_eq!(super::fill_due(&app.state).await.unwrap(), 1);
        sqlx::query("UPDATE tags SET franchise_checked_at = NULL WHERE name = 'miku'")
            .execute(app.db())
            .await
            .unwrap();

        // A round takes her, and works out that she has no franchise...
        let ids = super::claim_due(&app.state).await.unwrap();
        assert_eq!(ids.len(), 1);
        let mut conn = app.state.db.acquire().await.unwrap();
        let found = super::resolve(&mut conn, &ids).await.unwrap();
        assert!(found.is_empty());
        // ...while she is given one.
        let r = app
            .call(
                Method::POST,
                "/api/v1/tags/miku/implications",
                Some(&t),
                Some(json!({"implies": "vocaloid"})),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        super::store(&mut conn, &ids, &found).await.unwrap();
        drop(conn);
        assert_eq!(super::fill_due(&app.state).await.unwrap(), 1);
        assert_eq!(person(&app, &t, "miku").await["franchise"], "vocaloid");
        app.finish().await;
    }

    /// Regression: when marking the characters failed, a tag edit that
    /// was already saved answered 500.
    #[tokio::test]
    async fn a_saved_tag_edit_stays_saved_when_marking_fails() {
        let Some(app) = testing::app().await else {
            return;
        };
        let t = app.register("admin").await;
        app.upload_png(&t, 1, "vocaloid").await;
        for statement in [
            "CREATE FUNCTION refuse() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN RAISE EXCEPTION 'refused'; END $$",
            "CREATE TRIGGER refuse BEFORE UPDATE OF franchise_checked_at ON tags \
             FOR EACH STATEMENT EXECUTE FUNCTION refuse()",
        ] {
            sqlx::query(statement).execute(app.db()).await.unwrap();
        }
        patch_tag(&app, &t, "vocaloid", json!({"category": "copyright"})).await;
        let r = app.get("/api/v1/tags/vocaloid", &t).await.json();
        assert_eq!(r["tag"]["category"], "copyright");
        app.finish().await;
    }

    #[tokio::test]
    async fn explore_shows_people() {
        let Some(app) = testing::app().await else {
            return;
        };
        let t = app.register("admin").await;
        let posts = seed(&app, &t).await;
        let e = app.get("/api/v1/explore", &t).await.json();
        assert_eq!(names(&e["people"]), ["saber", "miku", "rin"]);
        assert_eq!(e["people"][0]["franchise"], "fate_series");
        assert!(e["people"][0]["face_url"].is_string());
        // Private: nothing is popular.
        assert_eq!(e["popular_characters"], json!([]));
        // A search counts the people on what it finds, its own tags left out.
        let e = app.get("/api/v1/explore?q=fate_series", &t).await.json();
        assert_eq!(names(&e["people"]), ["saber", "rin"]);
        assert_eq!(e["people"][0]["posts"], 2);
        assert!(e["people"][0]["face_url"].is_string());
        assert!(e["people"][1]["thumb_url"].is_string());
        let e = app.get("/api/v1/explore?q=saber", &t).await.json();
        assert_eq!(names(&e["people"]), Vec::<String>::new());
        // Her face is on a post the search doesn't find; it still shows.
        let e = app.get("/api/v1/explore?q=sky", &t).await.json();
        assert_eq!(names(&e["people"]), ["saber"]);
        assert!(e["people"][0]["face_url"].is_string());
        assert_eq!(e["people"][0]["thumb_url"], Value::Null);
        // The account's blacklist hides a character, or everyone of a franchise.
        let r = app
            .call(
                Method::PUT,
                "/api/v1/auth/blacklist",
                Some(&t),
                Some(json!({"entries": ["fate_series"]})),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK);
        let e = app.get("/api/v1/explore", &t).await.json();
        assert_eq!(names(&e["people"]), ["miku"]);
        let r = app.get("/api/v1/people", &t).await.json();
        assert_eq!(names(&r["people"]), ["miku"]);
        let r = app.get("/api/v1/franchises", &t).await.json();
        assert_eq!(
            (r["franchises"].clone(), r["without"].clone()),
            (json!([]), json!(1))
        );
        // Guests can't read a private server.
        let r = app.call(Method::GET, "/api/v1/people", None, None).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        assert!(!posts.is_empty());
        app.finish().await;
    }

    #[tokio::test]
    async fn popular_characters_on_a_public_server() {
        let Some(app) = testing::app_with(|c| c.mode = Some(Mode::Public)).await else {
            return;
        };
        let t = app.register("admin").await;
        let bob = app.register("bob").await;
        let posts = seed(&app, &t).await;
        let vote = |id: i64, token: &str, value: i64| {
            let token = token.to_owned();
            let app = &app;
            async move {
                let r = app
                    .call(
                        Method::PUT,
                        &format!("/api/v1/posts/{id}/vote"),
                        Some(&token),
                        Some(json!({ "value": value })),
                    )
                    .await;
                assert_eq!(r.status, StatusCode::OK);
            }
        };
        let (saber, saber_face, rin, miku) = (posts[0], posts[2], posts[3], posts[4]);
        vote(rin, &t, 1).await;
        vote(rin, &bob, 1).await;
        vote(saber_face, &bob, 1).await;
        vote(saber, &bob, 1).await;
        vote(saber, &t, -1).await;
        vote(miku, &bob, -1).await;

        let e = app.get("/api/v1/explore", &bob).await.json();
        let popular = &e["popular_characters"];
        // Rin's post has two upvotes; Saber's one, and one more that came to nothing.
        assert_eq!(names(popular), ["rin", "saber"]);
        assert_eq!(
            (popular[0]["votes"].clone(), popular[0]["posts"].clone()),
            (json!(2), json!(1))
        );
        assert_eq!(
            (popular[1]["votes"].clone(), popular[1]["posts"].clone()),
            (json!(1), json!(1))
        );
        assert_eq!(popular[1]["franchise"], "fate_series");
        assert!(popular[1]["face_url"].is_string());
        assert!(popular[0]["thumb_url"].is_string());
        // Outside the window it is gone.
        sqlx::query(
            "UPDATE post_votes SET voted_at = now() - interval '10 days' WHERE post_id = $1",
        )
        .bind(rin)
        .execute(app.db())
        .await
        .unwrap();
        let e = app.get("/api/v1/explore?days=7", &bob).await.json();
        assert_eq!(names(&e["popular_characters"]), ["saber"]);
        let e = app.get("/api/v1/explore?days=30", &bob).await.json();
        assert_eq!(names(&e["popular_characters"]), ["rin", "saber"]);
        // A search narrows it.
        let e = app.get("/api/v1/explore?days=30&q=rin", &bob).await.json();
        assert_eq!(names(&e["popular_characters"]), Vec::<String>::new());
        let e = app.get("/api/v1/explore?days=30&q=-rin", &bob).await.json();
        assert_eq!(names(&e["popular_characters"]), ["saber"]);
        let e = app
            .get("/api/v1/explore?days=30&q=fate_series", &bob)
            .await
            .json();
        assert_eq!(names(&e["popular_characters"]), ["rin", "saber"]);

        // Guests see it, through their own blacklist.
        let guest = |path: &'static str| {
            let app = &app;
            async move {
                let r = app.call(Method::GET, path, None, None).await;
                assert_eq!(r.status, StatusCode::OK, "{path}: {:?}", r.json());
                r.json()
            }
        };
        let e = guest("/api/v1/explore?days=30").await;
        assert_eq!(names(&e["popular_characters"]), ["rin", "saber"]);
        assert_eq!(names(&e["people"]), ["saber", "miku", "rin"]);
        let e = guest("/api/v1/explore?days=30&exclude=rin").await;
        assert_eq!(names(&e["popular_characters"]), ["saber"]);
        assert_eq!(names(&e["people"]), ["saber", "miku"]);
        let r = guest("/api/v1/people?exclude=fate_series").await;
        assert_eq!(names(&r["people"]), ["miku"]);
        let r = guest("/api/v1/franchises").await;
        assert_eq!(r["franchises"][0]["name"], "fate_series");
        app.finish().await;
    }
}
