//! The people in a post ("figures", `starbooru_core::figure`): saving them,
//! loading them for the viewer, and keeping them in step with the post's
//! tags. A figure's tags are on the post too; taking a tag off the post
//! takes it off the figures, and while a post has figures its count tags
//! (`1girl`, `solo`...) are worked out from them. A figure's face box gets
//! a crop of its own (`crate::faces`).

use axum::Json;
use axum::extract::{Path, State};
use sqlx::{PgExecutor, Postgres, Transaction};
use starbooru_core::tag::{MAX_TAGS_PER_POST, TagError};
use starbooru_core::{
    COSPLAY_TAG, FaceBox, Figure, FigureKind, FiguresUpdate, Post, Sent, Slot, TagCategory,
    clean_figures, cosplay_name, cosplay_tag, keep_unsent, required_tags,
};
use tracing::warn;

use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// `figure_tags.slot` of the character tag; the slots proper start at 2.
pub(super) const CHARACTER_SLOT: i16 = 0;
/// `figure_tags.slot` of the `name_(cosplay)` tag.
pub(super) const COSPLAY_SLOT: i16 = 1;
/// `figure_tags.slot` of the franchise, after the slots proper.
pub(super) const FRANCHISE_SLOT: i16 = 13;

/// A figure as stored, with the name of its face's crop once made.
struct Stored {
    figure: Figure,
    face_sha: Option<Vec<u8>>,
}

/// position, kind, the face box, its crop, then one of its tags.
type FigureRow = (
    i16,
    i16,
    Option<i16>,
    Option<i16>,
    Option<i16>,
    Option<i16>,
    Option<Vec<u8>>,
    Option<i16>,
    Option<String>,
);

/// A face box as stored, if it is one.
fn face_box(x: Option<i16>, y: Option<i16>, w: Option<i16>, h: Option<i16>) -> Option<FaceBox> {
    let edge = |v: Option<i16>| v.and_then(|v| u16::try_from(v).ok());
    let face = FaceBox {
        x: edge(x)?,
        y: edge(y)?,
        w: edge(w)?,
        h: edge(h)?,
    };
    face.is_valid().then_some(face)
}

async fn load_stored(db: impl PgExecutor<'_>, post_id: i64) -> AppResult<Vec<Stored>> {
    let rows: Vec<FigureRow> = sqlx::query_as(
        "SELECT f.position, f.kind, f.face_x, f.face_y, f.face_w, f.face_h, f.face_sha, \
                ft.slot, t.name FROM figures f \
         LEFT JOIN figure_tags ft ON ft.figure_id = f.id \
         LEFT JOIN tags t ON t.id = ft.tag_id \
         WHERE f.post_id = $1 ORDER BY f.position, ft.slot, ft.position",
    )
    .bind(post_id)
    .fetch_all(db)
    .await?;
    let mut out: Vec<(i16, Stored)> = Vec::new();
    for (position, kind, x, y, w, h, face_sha, slot, name) in rows {
        if out.last().is_none_or(|(p, _)| *p != position) {
            let face = face_box(x, y, w, h);
            let figure = Figure {
                kind: FigureKind::from_db(kind).unwrap_or_default(),
                face,
                ..Figure::default()
            };
            let face_sha = face_sha.filter(|_| face.is_some());
            out.push((position, Stored { figure, face_sha }));
        }
        let (Some((_, stored)), Some(slot), Some(name)) = (out.last_mut(), slot, name) else {
            continue;
        };
        let figure = &mut stored.figure;
        match slot {
            CHARACTER_SLOT => figure.character = Some(name),
            COSPLAY_SLOT => figure.cosplay = Some(cosplay_name(&name).to_owned()),
            FRANCHISE_SLOT => figure.franchise = Some(name),
            slot => {
                let slot = Slot::from_db(slot).unwrap_or(Slot::Other);
                figure.tags.entry(slot).or_default().push(name);
            }
        }
    }
    Ok(out.into_iter().map(|(_, s)| s).collect())
}

/// A post's figures, in order.
pub(super) async fn load(db: impl PgExecutor<'_>, post_id: i64) -> AppResult<Vec<Figure>> {
    Ok(load_stored(db, post_id)
        .await?
        .into_iter()
        .map(|s| s.figure)
        .collect())
}

/// A post's figures as answers give them: with the signed URL of each
/// face's crop that is ready.
pub(super) async fn for_answer(state: &AppState, post_id: i64) -> AppResult<Vec<Figure>> {
    Ok(load_stored(&state.db, post_id)
        .await?
        .into_iter()
        .map(|s| Figure {
            face_url: s
                .face_sha
                .map(|sha| crate::faces::url(state, &hex::encode(sha))),
            ..s.figure
        })
        .collect())
}

/// Takes tags off a post's figures that the post no longer has. The
/// characters of the figures that lost one, the removed ones included, get
/// their franchise and faces worked out again.
pub(super) async fn prune(
    tx: &mut Transaction<'_, Postgres>,
    post_id: i64,
    tag_ids: &[i64],
) -> AppResult<()> {
    // The UPDATE still sees the deleted rows: they share one snapshot.
    sqlx::query(
        "WITH gone AS (DELETE FROM figure_tags ft USING figures f \
             WHERE ft.figure_id = f.id AND f.post_id = $1 AND NOT (ft.tag_id = ANY($2)) \
             RETURNING ft.figure_id) \
         UPDATE tags SET franchise_checked_at = NULL \
         WHERE franchise_checked_at IS NOT NULL AND id IN (SELECT ft.tag_id FROM figure_tags ft \
             WHERE ft.slot = $3 AND ft.figure_id IN (SELECT figure_id FROM gone))",
    )
    .bind(post_id)
    .bind(tag_ids)
    .bind(CHARACTER_SLOT)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Every tag a post's figures put on it, the count tags included.
async fn derived(tx: &mut Transaction<'_, Postgres>, post_id: i64) -> AppResult<Vec<String>> {
    let figures = load(&mut **tx, post_id).await?;
    let mut names: Vec<String> = sqlx::query_scalar(
        "SELECT t.name FROM figures f JOIN figure_tags ft ON ft.figure_id = f.id \
         JOIN tags t ON t.id = ft.tag_id WHERE f.post_id = $1",
    )
    .bind(post_id)
    .fetch_all(&mut **tx)
    .await?;
    names.extend(required_tags(&figures));
    Ok(names)
}

/// What saving people leaves to do once the transaction is in.
#[must_use = "settle it after the commit"]
pub(crate) struct Saved {
    /// Crops the post's figures had and no longer name.
    dropped: Vec<Vec<u8>>,
    /// Faces are waiting for their crops.
    faces_due: bool,
}

impl Saved {
    /// Removes the crops nothing names any more and wakes the tasks that
    /// make crops and work out franchises. Call after the commit.
    pub(crate) async fn settle(self, state: &AppState) {
        if let Err(e) = crate::faces::release(state, self.dropped).await {
            warn!("face crops no figure uses were left behind: {e}");
        }
        if self.faces_due {
            state.faces.notify_one();
        }
        state.franchises.notify_one();
    }
}

/// Fills in the franchise of each figure named here for the first time
/// without one: the franchise the collection gives its character, or for
/// a cosplay the character they dress as. A figure of the same person was
/// here before keeps what it has, so a franchise taken off stays off.
async fn fill_franchises(
    tx: &mut Transaction<'_, Postgres>,
    figures: &mut [Figure],
    before: &[Stored],
) -> AppResult<()> {
    let key = |f: &Figure| f.cosplay.clone().or_else(|| f.character.clone());
    let new = |f: &Figure| {
        f.franchise.is_none()
            && !before
                .iter()
                .any(|b| b.figure.character == f.character && b.figure.cosplay == f.cosplay)
    };
    let wanted: Vec<String> = figures.iter().filter(|f| new(f)).filter_map(key).collect();
    if wanted.is_empty() {
        return Ok(());
    }
    let found = super::people::franchises_by_name(tx, &wanted).await?;
    for figure in figures.iter_mut() {
        if new(figure)
            && let Some(franchise) = key(figure).and_then(|k| found.get(&k))
        {
            figure.franchise = Some(franchise.clone());
        }
    }
    Ok(())
}

/// Replaces a live, locked post's figures and updates its tags to match:
/// tags only the old figures gave are removed, the new figures' tags are
/// added. Names are resolved through aliases and missing tags created;
/// character tags become characters, new franchise tags copyrights, and
/// new `name_(cosplay)` tags go under `cosplay`. A face box the post had
/// keeps its crop. With `fill`, figures named now get their character's
/// franchise when they give none.
pub(crate) async fn replace(
    tx: &mut Transaction<'_, Postgres>,
    post_id: i64,
    figures: Vec<Figure>,
    fill: bool,
) -> AppResult<Saved> {
    let mut figures = clean_figures(figures)?;
    let before = load_stored(&mut **tx, post_id).await?;
    if fill {
        fill_franchises(tx, &mut figures, &before).await?;
    }
    let old = derived(tx, post_id).await?;
    let franchises: Vec<String> = figures.iter().filter_map(|f| f.franchise.clone()).collect();
    if !franchises.is_empty() {
        // Made as copyrights when new; an existing tag keeps its category.
        let franchises = super::tags::canonical_names(&mut **tx, &franchises).await?;
        sqlx::query(
            "INSERT INTO tags (name, category) SELECT unnest($1::text[]), $2 \
             ON CONFLICT (name) DO NOTHING",
        )
        .bind(&franchises)
        .bind(TagCategory::Copyright.to_db())
        .execute(&mut **tx)
        .await?;
    }
    let names: Vec<String> = figures.iter().flat_map(Figure::tag_names).collect();
    let tags = super::tags::by_name(tx, &names).await?;
    let id_of = |name: &str| tags.get(name).map(|(id, _)| *id);

    sqlx::query("DELETE FROM figures WHERE post_id = $1")
        .bind(post_id)
        .execute(&mut **tx)
        .await?;
    let mut characters = Vec::new();
    let mut cosplays = Vec::new();
    let mut kept_faces: Vec<Vec<u8>> = Vec::new();
    let mut faces_due = false;
    for (position, figure) in (0i16..).zip(&figures) {
        // The same box on the same picture is the same crop.
        let face_sha = figure.face.and_then(|face| {
            before
                .iter()
                .find(|b| b.figure.face == Some(face) && b.face_sha.is_some())
                .and_then(|b| b.face_sha.clone())
        });
        faces_due |= figure.face.is_some() && face_sha.is_none();
        kept_faces.extend(face_sha.iter().cloned());
        let edge = |pick: fn(FaceBox) -> u16| {
            figure
                .face
                .map(|f| i16::try_from(pick(f)).unwrap_or(i16::MAX))
        };
        let figure_id: i64 = sqlx::query_scalar(
            "INSERT INTO figures (post_id, position, kind, face_x, face_y, face_w, face_h, \
             face_sha) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
        )
        .bind(post_id)
        .bind(position)
        .bind(figure.kind.to_db())
        .bind(edge(|f| f.x))
        .bind(edge(|f| f.y))
        .bind(edge(|f| f.w))
        .bind(edge(|f| f.h))
        .bind(face_sha)
        .fetch_one(&mut **tx)
        .await?;
        let mut rows: Vec<(i64, i16)> = Vec::new();
        let character = figure.character.as_deref().and_then(id_of);
        let cosplay = figure.cosplay.as_deref().map(cosplay_tag);
        let cosplay = cosplay.as_deref().and_then(id_of);
        let franchise = figure.franchise.as_deref().and_then(id_of);
        rows.extend(character.map(|id| (id, CHARACTER_SLOT)));
        rows.extend(cosplay.map(|id| (id, COSPLAY_SLOT)));
        rows.extend(franchise.map(|id| (id, FRANCHISE_SLOT)));
        characters.extend(character);
        cosplays.extend(cosplay);
        for (slot, names) in &figure.tags {
            rows.extend(
                names
                    .iter()
                    .filter_map(|n| id_of(n))
                    .map(|id| (id, slot.to_db())),
            );
        }
        // Two names can be aliases of one tag: the first place wins.
        let mut seen = Vec::with_capacity(rows.len());
        rows.retain(|(id, _)| {
            let new = !seen.contains(id);
            seen.push(*id);
            new
        });
        let (ids, slots): (Vec<i64>, Vec<i16>) = rows.into_iter().unzip();
        sqlx::query(
            "INSERT INTO figure_tags (figure_id, tag_id, slot, position) \
             SELECT $1, t.id, t.slot, (t.ord - 1)::smallint \
             FROM unnest($2::bigint[], $3::smallint[]) WITH ORDINALITY AS t(id, slot, ord)",
        )
        .bind(figure_id)
        .bind(&ids)
        .bind(&slots)
        .execute(&mut **tx)
        .await?;
    }
    if !characters.is_empty() {
        sqlx::query("UPDATE tags SET category = $2 WHERE id = ANY($1) AND category = $3")
            .bind(&characters)
            .bind(TagCategory::Character.to_db())
            .bind(TagCategory::General.to_db())
            .execute(&mut **tx)
            .await?;
    }
    let parent = if cosplays.is_empty() {
        None
    } else {
        let found = super::tags::by_name(tx, &[COSPLAY_TAG.to_owned()]).await?;
        found.get(COSPLAY_TAG).map(|(id, _)| *id)
    };
    if let Some(parent) = parent {
        // Under `cosplay` unless placed already, or `cosplay` sits below it.
        sqlx::query(
            "WITH RECURSIVE c AS (SELECT id, parent_id, 0 AS depth FROM tags WHERE id = $2 \
                 UNION ALL SELECT t.id, t.parent_id, c.depth + 1 FROM tags t \
                 JOIN c ON t.id = c.parent_id WHERE c.depth < 32) \
             UPDATE tags SET parent_id = $2 \
             WHERE id = ANY($1) AND parent_id IS NULL AND id NOT IN (SELECT id FROM c)",
        )
        .bind(&cosplays)
        .bind(parent)
        .execute(&mut **tx)
        .await?;
    }
    // The characters' franchises are worked out again, as their figures changed.
    let named: Vec<String> = before
        .iter()
        .filter_map(|b| b.figure.character.clone())
        .collect();
    if !named.is_empty() || !characters.is_empty() {
        sqlx::query(
            "UPDATE tags SET franchise_checked_at = NULL \
             WHERE (id = ANY($1) OR name = ANY($2)) AND franchise_checked_at IS NOT NULL",
        )
        .bind(&characters)
        .bind(&named)
        .execute(&mut **tx)
        .await?;
    }

    // The post's tags: what only the old figures gave goes, the new figures' come.
    let new = derived(tx, post_id).await?;
    let gone: Vec<String> = old.into_iter().filter(|n| !new.contains(n)).collect();
    let current: Vec<String> = sqlx::query_scalar(
        "SELECT t.name FROM posts p JOIN tags t ON t.id = ANY(p.tag_ids) WHERE p.id = $1",
    )
    .bind(post_id)
    .fetch_all(&mut **tx)
    .await?;
    let mut names: Vec<String> = current.into_iter().filter(|n| !gone.contains(n)).collect();
    for name in new {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if names.len() > MAX_TAGS_PER_POST {
        return Err(TagError::TooMany.into());
    }
    super::posts::set_tags(tx, post_id, &names).await?;
    let dropped = before
        .into_iter()
        .filter_map(|b| b.face_sha)
        .filter(|sha| !kept_faces.contains(sha))
        .collect();
    Ok(Saved { dropped, faces_due })
}

/// `PUT /posts/{id}/figures`: replaces the post's figures and returns it.
/// A figure that leaves out `franchise` or `face`, as apps from before them
/// do, keeps what the same person had (`starbooru_core::keep_unsent`).
pub async fn put(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> AppResult<Json<Post>> {
    let sent = Sent::read(&body);
    let update: FiguresUpdate = serde_json::from_value(body)
        .map_err(|e| AppError::BadRequest(format!("the people can't be read: {e}")))?;
    let mut figures = clean_figures(update.figures)?;
    let mut tx = state.db.begin().await?;
    super::posts::lock_live(&mut tx, id).await?;
    let before = load(&mut *tx, id).await?;
    keep_unsent(&mut figures, &sent, &before);
    let saved = replace(&mut tx, id, figures, true).await?;
    tx.commit().await?;
    saved.settle(&state).await;
    Ok(Json(super::posts::load_full(&state, user.id, id).await?))
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use serde_json::{Value, json};

    use crate::testing::{self, TestApp};

    async fn put_figures(app: &TestApp, t: &str, id: i64, figures: Value) -> testing::Resp {
        app.call(
            Method::PUT,
            &format!("/api/v1/posts/{id}/figures"),
            Some(t),
            Some(json!({ "figures": figures })),
        )
        .await
    }

    fn sorted_tags(post: &Value) -> Vec<String> {
        let mut tags: Vec<String> = post["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().to_owned())
            .collect();
        tags.sort();
        tags
    }

    async fn found(app: &TestApp, t: &str, q: &str) -> Vec<i64> {
        let r = app.get(&format!("/api/v1/posts?q={q}"), t).await;
        assert_eq!(r.status, StatusCode::OK, "{q}: {:?}", r.json());
        let mut ids: Vec<i64> = r.json()["posts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_i64().unwrap())
            .collect();
        ids.sort_unstable();
        ids
    }

    #[tokio::test]
    async fn figures_keep_the_post_tags_in_step() {
        let Some(app) = testing::app().await else {
            return;
        };
        let t = app.register("admin").await;
        let id = app.upload_png(&t, 1, "sky 2girls solo").await;
        let figures = json!([
            {"kind": "girl", "character": "char:Saber",
             "tags": {"hair": ["blonde_hair"], "shoes": ["boots"]}},
            {"kind": "boy", "cosplay": "link_(cosplay)", "tags": {"head": ["hat"]}},
        ]);
        let r = put_figures(&app, &t, id, figures).await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        let post = r.json();
        assert_eq!(
            sorted_tags(&post),
            [
                "1boy",
                "1girl",
                "blonde_hair",
                "boots",
                "cosplay",
                "hat",
                "link_(cosplay)",
                "saber",
                "sky"
            ]
        );
        assert_eq!(
            post["figures"],
            json!([
                {"kind": "girl", "character": "saber", "franchise": null,
                 "tags": {"hair": ["blonde_hair"], "shoes": ["boots"]}, "face": null},
                {"kind": "boy", "cosplay": "link", "franchise": null,
                 "tags": {"head": ["hat"]}, "face": null},
            ])
        );
        let saber = app.get("/api/v1/tags/saber", &t).await.json();
        assert_eq!(saber["tag"]["category"], "character");
        let link = app.get("/api/v1/tags/link_(cosplay)", &t).await.json();
        assert_eq!(link["ancestors"][0]["name"], "cosplay");

        // Taking a tag off the post takes it off its figure; the counts come back.
        let r = app
            .call(
                Method::PATCH,
                &format!("/api/v1/posts/{id}"),
                Some(&t),
                Some(json!({"tags": ["sky", "saber", "blonde_hair", "hat", "link_(cosplay)"]})),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK);
        let post = r.json();
        assert!(!sorted_tags(&post).contains(&"boots".to_owned()));
        assert!(sorted_tags(&post).contains(&"1girl".to_owned()));
        assert!(sorted_tags(&post).contains(&"cosplay".to_owned()));
        assert_eq!(post["figures"][0]["tags"], json!({"hair": ["blonde_hair"]}));

        // One figure left: solo, and what only the boy gave goes.
        let r = put_figures(&app, &t, id, json!([post["figures"][0].clone()])).await;
        assert_eq!(
            sorted_tags(&r.json()),
            ["1girl", "blonde_hair", "saber", "sky", "solo"]
        );
        // None left: back to the post's own tags.
        let r = put_figures(&app, &t, id, json!([])).await;
        assert_eq!(sorted_tags(&r.json()), ["sky"]);
        assert!(r.json().get("figures").is_none());
        // Without figures, count tags are ordinary tags.
        let r = app
            .call(
                Method::PATCH,
                &format!("/api/v1/posts/{id}"),
                Some(&t),
                Some(json!({"tags": ["sky", "3girls"]})),
            )
            .await;
        assert_eq!(sorted_tags(&r.json()), ["3girls", "sky"]);

        // Limits, and posts in the bin.
        let many = json!(vec![
            json!({"kind": "other"});
            starbooru_core::MAX_FIGURES + 1
        ]);
        let r = put_figures(&app, &t, id, many).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        let r = put_figures(
            &app,
            &t,
            id,
            json!([{"kind": "girl", "tags": {"hair": ["-x"]}}]),
        )
        .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        let r = put_figures(&app, &t, 999_999, json!([])).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
        let r = app
            .call(
                Method::DELETE,
                &format!("/api/v1/posts/{id}"),
                Some(&t),
                None,
            )
            .await;
        assert!(r.status.is_success());
        let r = put_figures(&app, &t, id, json!([{"kind": "girl"}])).await;
        assert_eq!(r.status, StatusCode::CONFLICT);
        app.finish().await;
    }

    #[tokio::test]
    async fn searches_find_one_figure_with_every_tag() {
        let Some(app) = testing::app().await else {
            return;
        };
        let t = app.register("admin").await;
        let together = app.upload_png(&t, 1, "").await;
        let apart = app.upload_png(&t, 2, "").await;
        let plain = app.upload_png(&t, 3, "blonde_hair boots").await;
        let r = put_figures(
            &app,
            &t,
            together,
            json!([{"kind": "girl", "tags": {"hair": ["blonde_hair"], "shoes": ["boots"]}}]),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK);
        let r = put_figures(
            &app,
            &t,
            apart,
            json!([
                {"kind": "girl", "tags": {"hair": ["blonde_hair"]}},
                {"kind": "boy", "tags": {"shoes": ["boots"]}},
            ]),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK);
        let r = app
            .call(
                Method::PATCH,
                "/api/v1/tags/boots",
                Some(&t),
                Some(json!({"parent": "footwear"})),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK);

        // Plain tags still find every post that has them.
        assert_eq!(
            found(&app, &t, "blonde_hair+boots").await,
            [together, apart, plain]
        );
        assert_eq!(found(&app, &t, "%7Bblonde_hair+boots%7D").await, [together]);
        // A parent stands for its tree; a kind narrows it down.
        assert_eq!(
            found(&app, &t, "%7Bgirl+footwear+blonde_hair%7D").await,
            [together]
        );
        assert_eq!(found(&app, &t, "%7Bboy+footwear%7D").await, [apart]);
        assert_eq!(found(&app, &t, "%7Bboy%7D").await, [apart]);
        assert_eq!(found(&app, &t, "%7Bother%7D").await, Vec::<i64>::new());
        // Exclusion: no figure matches.
        assert_eq!(
            found(&app, &t, "-%7Bblonde_hair+boots%7D").await,
            [apart, plain]
        );
        // Unknown tags: nothing matches, and nothing is excluded.
        assert_eq!(
            found(&app, &t, "%7Bblonde_hair+nowhere%7D").await,
            Vec::<i64>::new()
        );
        assert_eq!(
            found(&app, &t, "-%7Bnowhere%7D").await,
            [together, apart, plain]
        );
        let r = app.get("/api/v1/posts?q=%7B%7D", &t).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        app.finish().await;
    }

    #[tokio::test]
    async fn an_edit_keeps_the_people() {
        let Some(app) = testing::app().await else {
            return;
        };
        let t = app.register("admin").await;
        let id = app.upload_png(&t, 1, "sky").await;
        let figures = json!([{"kind": "boy", "tags": {"hair": ["blonde_hair"]}}]);
        assert_eq!(put_figures(&app, &t, id, figures.clone()).await.status, 200);
        let r = app
            .call(
                Method::POST,
                &format!("/api/v1/posts/{id}/edit"),
                Some(&t),
                Some(json!({"rotate": 90})),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        let copy = r.json();
        assert_ne!(copy["id"].as_i64(), Some(id));
        assert_eq!(
            copy["figures"],
            json!([{"kind": "boy", "franchise": null, "tags": {"hair": ["blonde_hair"]},
                    "face": null}])
        );
        assert_eq!(sorted_tags(&copy), ["1boy", "blonde_hair", "sky", "solo"]);
        app.finish().await;
    }

    /// Uploads a `w` × `h` picture and returns the new post's id.
    async fn upload_sized(app: &TestApp, t: &str, w: u32, h: u32, seed: u32) -> i64 {
        let r = app
            .upload(t, &testing::png(w, h, seed), &[("tags", "sky")])
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        r.json()["id"].as_i64().unwrap()
    }

    /// The crop names the figures of a post hold, in order.
    async fn crops(app: &TestApp, id: i64) -> Vec<Option<String>> {
        let names: Vec<Option<Vec<u8>>> =
            sqlx::query_scalar("SELECT face_sha FROM figures WHERE post_id = $1 ORDER BY position")
                .bind(id)
                .fetch_all(app.db())
                .await
                .unwrap();
        names.into_iter().map(|n| n.map(hex::encode)).collect()
    }

    #[tokio::test]
    async fn people_have_a_franchise_and_a_face() {
        let Some(app) = testing::app().await else {
            return;
        };
        let t = app.register("admin").await;
        let id = upload_sized(&app, &t, 200, 100, 1).await;
        let face = json!({"x": 100, "y": 200, "w": 250, "h": 500});
        let r = put_figures(
            &app,
            &t,
            id,
            json!([
                {"kind": "girl", "character": "saber", "franchise": "Fate Series", "face": face},
                {"kind": "girl", "character": "rin", "franchise": "sky", "face": null},
            ]),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        let post = r.json();
        assert!(sorted_tags(&post).contains(&"fate_series".to_owned()));
        assert_eq!(post["figures"][0]["franchise"], "fate_series");
        assert_eq!(post["figures"][0]["face"], face);
        // The crop is made in the background, so it isn't there yet.
        assert!(post["figures"][0].get("face_url").is_none());
        // A new franchise is a copyright; a tag already in use keeps its category.
        let fate = app.get("/api/v1/tags/fate_series", &t).await.json();
        assert_eq!(fate["tag"]["category"], "copyright");
        let sky = app.get("/api/v1/tags/sky", &t).await.json();
        assert_eq!(sky["tag"]["category"], "general");

        assert_eq!(crate::faces::make_due(&app.state).await.unwrap(), 1);
        let post = app.get(&format!("/api/v1/posts/{id}"), &t).await.json();
        let url = post["figures"][0]["face_url"].as_str().unwrap().to_owned();
        assert!(url.starts_with("/media/face/"), "{url}");
        assert!(post["figures"][1].get("face_url").is_none());
        let r = app.call(Method::GET, &url, None, None).await;
        assert_eq!(r.status, StatusCode::OK);
        let crop = crate::storage::decode(&r.body).unwrap();
        assert_eq!(
            (crop.width(), crop.height()),
            (starbooru_core::FACE_SIZE, starbooru_core::FACE_SIZE)
        );
        let first = crops(&app, id).await;

        // An app from before franchises and faces keeps both; the same box
        // keeps its crop, even moved to another place.
        let r = put_figures(
            &app,
            &t,
            id,
            json!([
                {"kind": "girl", "character": "rin"},
                {"kind": "girl", "character": "saber", "tags": {"hair": ["blonde_hair"]}},
            ]),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        let post = r.json();
        assert_eq!(post["figures"][0]["franchise"], "sky");
        assert_eq!(post["figures"][1]["franchise"], "fate_series");
        assert_eq!(post["figures"][1]["face"], face);
        assert_eq!(crops(&app, id).await, [None, first[0].clone()]);
        assert_eq!(crate::faces::make_due(&app.state).await.unwrap(), 0);

        // A new box gets a new crop and the old one goes.
        let old = first[0].clone().unwrap();
        let moved = json!({"x": 0, "y": 0, "w": 500, "h": 1000});
        let r = put_figures(
            &app,
            &t,
            id,
            json!([{"kind": "girl", "character": "saber", "franchise": "fate_series",
                    "face": moved}]),
        )
        .await;
        assert_eq!(r.json()["figures"][0]["face"], moved);
        assert!(!app.state.storage.has_face(&old));
        assert_eq!(crate::faces::make_due(&app.state).await.unwrap(), 1);
        let now = crops(&app, id).await;
        assert!(
            now[0]
                .as_deref()
                .is_some_and(|n| n != old && app.state.storage.has_face(n))
        );
        // Taken off with null; its crop goes too.
        let r = put_figures(
            &app,
            &t,
            id,
            json!([{"kind": "girl", "character": "saber", "franchise": null, "face": null}]),
        )
        .await;
        let post = r.json();
        assert_eq!(post["figures"][0]["face"], Value::Null);
        assert_eq!(post["figures"][0]["franchise"], Value::Null);
        assert!(!sorted_tags(&post).contains(&"fate_series".to_owned()));
        assert!(!app.state.storage.has_face(now[0].as_deref().unwrap()));

        // A box outside the picture is refused.
        for bad in [
            json!({"x": 900, "y": 0, "w": 200, "h": 10}),
            json!({"x": 0, "y": 0, "w": 0, "h": 10}),
            json!({"x": -1, "y": 0, "w": 5, "h": 5}),
        ] {
            let r = put_figures(&app, &t, id, json!([{"kind": "girl", "face": bad}])).await;
            assert_eq!(r.status, StatusCode::BAD_REQUEST, "{bad}");
        }
        app.finish().await;
    }

    #[tokio::test]
    async fn a_character_brings_their_franchise() {
        let Some(app) = testing::app().await else {
            return;
        };
        let t = app.register("admin").await;
        let id = app.upload_png(&t, 1, "").await;
        let patch = |name: &'static str, body: Value| {
            let app = &app;
            let t = &t;
            async move {
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
        };
        patch("fate_series", json!({"category": "copyright"})).await;
        let r = app
            .call(
                Method::POST,
                "/api/v1/tags/saber/implications",
                Some(&t),
                Some(json!({"implies": "fate_series"})),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        // A parent that is a copyright counts too.
        patch("hatsune_miku", json!({"parent": "vocaloid"})).await;
        patch("vocaloid", json!({"category": "copyright"})).await;

        let r = put_figures(
            &app,
            &t,
            id,
            json!([
                {"kind": "girl", "character": "saber"},
                {"kind": "boy", "cosplay": "hatsune_miku"},
                {"kind": "girl", "character": "nobody"},
            ]),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        let post = r.json();
        assert_eq!(post["figures"][0]["franchise"], "fate_series");
        // A cosplay's franchise is the costume's.
        assert_eq!(post["figures"][1]["franchise"], "vocaloid");
        assert_eq!(post["figures"][2]["franchise"], Value::Null);
        assert!(sorted_tags(&post).contains(&"vocaloid".to_owned()));

        // Taken off, it stays off for that person.
        let r = put_figures(
            &app,
            &t,
            id,
            json!([{"kind": "girl", "character": "saber", "franchise": null, "face": null}]),
        )
        .await;
        assert_eq!(r.json()["figures"][0]["franchise"], Value::Null);
        app.finish().await;
    }

    /// What `franchise: null` means, as `api.md` says: fill one in for a
    /// person new to the post, clear it for one already there, and an app
    /// that leaves the key out keeps what the person had.
    #[tokio::test]
    async fn null_fills_a_new_person_in_and_clears_one_already_there() {
        let Some(app) = testing::app().await else {
            return;
        };
        let t = app.register("admin").await;
        let id = app.upload_png(&t, 1, "").await;
        let r = app
            .call(
                Method::POST,
                "/api/v1/tags/saber/implications",
                Some(&t),
                Some(json!({"implies": "fate_series"})),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        let r = app
            .call(
                Method::PATCH,
                "/api/v1/tags/fate_series",
                Some(&t),
                Some(json!({"category": "copyright"})),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        let saber = |franchise: Option<Value>| {
            let mut figure = json!({"kind": "girl", "character": "saber", "face": null});
            if let Some(franchise) = franchise {
                figure["franchise"] = franchise;
            }
            json!([figure])
        };
        // New here: filled in, though the key came with null.
        let r = put_figures(&app, &t, id, saber(Some(Value::Null))).await;
        assert_eq!(r.json()["figures"][0]["franchise"], "fate_series");
        // Already here: null clears it, and saving again keeps it off.
        let r = put_figures(&app, &t, id, saber(Some(Value::Null))).await;
        assert_eq!(r.json()["figures"][0]["franchise"], Value::Null);
        let r = put_figures(&app, &t, id, saber(Some(Value::Null))).await;
        assert_eq!(r.json()["figures"][0]["franchise"], Value::Null);
        // An older app leaves the key out: what the person had stays.
        let r = put_figures(&app, &t, id, saber(None)).await;
        assert_eq!(r.json()["figures"][0]["franchise"], Value::Null);
        let r = put_figures(&app, &t, id, saber(Some(json!("type_moon")))).await;
        assert_eq!(r.json()["figures"][0]["franchise"], "type_moon");
        let r = put_figures(&app, &t, id, saber(None)).await;
        assert_eq!(r.json()["figures"][0]["franchise"], "type_moon");
        app.finish().await;
    }

    #[tokio::test]
    async fn faces_follow_an_edit_and_a_purge() {
        let Some(app) = testing::app().await else {
            return;
        };
        let t = app.register("admin").await;
        let id = upload_sized(&app, &t, 200, 100, 3).await;
        let r = put_figures(
            &app,
            &t,
            id,
            json!([
                {"kind": "girl", "face": {"x": 100, "y": 200, "w": 200, "h": 300}},
                {"kind": "boy", "face": {"x": 800, "y": 0, "w": 100, "h": 100}},
            ]),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        assert_eq!(crate::faces::make_due(&app.state).await.unwrap(), 2);

        // Turned a quarter: the top left goes to the top right.
        let r = app
            .call(
                Method::POST,
                &format!("/api/v1/posts/{id}/edit"),
                Some(&t),
                Some(json!({"rotate": 90})),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        let turned = r.json();
        assert_eq!(
            turned["figures"][0]["face"],
            json!({"x": 500, "y": 100, "w": 300, "h": 200})
        );
        assert_eq!(
            turned["figures"][1]["face"],
            json!({"x": 900, "y": 800, "w": 100, "h": 100})
        );
        // Cropped to the left half: the boy's face is left out.
        let r = app
            .call(
                Method::POST,
                &format!("/api/v1/posts/{id}/edit"),
                Some(&t),
                Some(json!({"crop": {"x": 0, "y": 0, "width": 100, "height": 100}})),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        let cropped = r.json();
        assert_eq!(
            cropped["figures"][0]["face"],
            json!({"x": 200, "y": 200, "w": 400, "h": 300})
        );
        assert_eq!(cropped["figures"][1]["face"], Value::Null);
        assert_eq!(crate::faces::make_due(&app.state).await.unwrap(), 3);

        // Purged, a post takes its crops with it.
        let copy = turned["id"].as_i64().unwrap();
        let names: Vec<String> = crops(&app, copy).await.into_iter().flatten().collect();
        assert_eq!(names.len(), 2);
        let r = app
            .call(
                Method::DELETE,
                &format!("/api/v1/posts/{copy}"),
                Some(&t),
                None,
            )
            .await;
        assert!(r.status.is_success());
        let r = app
            .call(
                Method::POST,
                &format!("/api/v1/posts/{copy}/purge"),
                Some(&t),
                None,
            )
            .await;
        assert!(r.status.is_success(), "{:?}", r.json());
        for name in &names {
            assert!(!app.state.storage.has_face(name));
        }
        // The original's are still there.
        for name in crops(&app, id).await.into_iter().flatten() {
            assert!(app.state.storage.has_face(&name));
        }

        // A crop file no figure names goes at the sweep; a named one whose file
        // is missing is made again.
        app.state
            .storage
            .write_face(&"ab".repeat(32), b"x")
            .unwrap();
        assert_eq!(crate::faces::sweep(&app.state).await.unwrap(), 1);
        // The cropped copy's girl is the same pixels, so the same file.
        let first = crops(&app, id).await[0].clone().unwrap();
        let copy = cropped["id"].as_i64().unwrap();
        assert_eq!(crops(&app, copy).await, [Some(first.clone()), None]);
        app.state.storage.remove_face(&first).unwrap();
        assert_eq!(crate::faces::forget_missing(&app.state).await.unwrap(), 2);
        assert_eq!(crate::faces::make_due(&app.state).await.unwrap(), 2);
        assert!(app.state.storage.has_face(&first));
        app.finish().await;
    }

    /// Regression: an original the server may not read failed the whole
    /// round, which then started again from the same post, so no later post
    /// ever got its crops.
    #[tokio::test]
    async fn an_unreadable_picture_fails_only_its_own_faces() {
        use std::os::unix::fs::PermissionsExt;
        let Some(app) = testing::app().await else {
            return;
        };
        let t = app.register("admin").await;
        let first = upload_sized(&app, &t, 200, 100, 7).await;
        let second = upload_sized(&app, &t, 200, 100, 8).await;
        for id in [first, second] {
            let face = json!({"x": 100, "y": 200, "w": 250, "h": 500});
            let r = put_figures(&app, &t, id, json!([{"kind": "girl", "face": face}])).await;
            assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        }
        let (sha, ext): (Vec<u8>, String) =
            sqlx::query_as("SELECT sha256, ext FROM posts WHERE id = $1")
                .bind(first)
                .fetch_one(app.db())
                .await
                .unwrap();
        let original = app
            .state
            .storage
            .original_path(&hex::encode(sha), &ext)
            .unwrap();
        std::fs::set_permissions(&original, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root reads it anyway: nothing to test then.
        if std::fs::read(&original).is_ok() {
            return app.finish().await;
        }
        let made = crate::faces::make_due(&app.state).await;
        std::fs::set_permissions(&original, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(made.unwrap(), 1);
        assert_eq!(crops(&app, first).await, [None]);
        assert!(crops(&app, second).await[0].is_some());
        let failed: bool = sqlx::query_scalar("SELECT face_failed FROM figures WHERE post_id = $1")
            .bind(first)
            .fetch_one(app.db())
            .await
            .unwrap();
        assert!(failed);
        app.finish().await;
    }

    #[tokio::test]
    async fn a_post_stays_under_the_tag_limit() {
        let Some(app) = testing::app().await else {
            return;
        };
        let t = app.register("admin").await;
        let id = app.upload_png(&t, 1, "sky").await;
        let figures: Vec<Value> = (0..20)
            .map(|f| {
                let tags: Vec<String> = (0..30).map(|i| format!("t{f}_{i}")).collect();
                json!({"kind": "other", "tags": {"other": tags}})
            })
            .collect();
        let r = put_figures(&app, &t, id, json!(figures)).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json());
        // Nothing was saved.
        let post = app.get(&format!("/api/v1/posts/{id}"), &t).await.json();
        assert_eq!(sorted_tags(&post), ["sky"]);
        assert!(post.get("figures").is_none());
        app.finish().await;
    }

    #[tokio::test]
    async fn merging_an_alias_moves_figure_tags() {
        let Some(app) = testing::app().await else {
            return;
        };
        let t = app.register("admin").await;
        let id = app.upload_png(&t, 1, "").await;
        let r = put_figures(
            &app,
            &t,
            id,
            json!([{"kind": "girl", "tags": {"hair": ["blond_hair", "long_hair"]}}]),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK);
        let r = app
            .call(
                Method::POST,
                "/api/v1/tags/blonde_hair/aliases",
                Some(&t),
                Some(json!({"alias": "blond_hair"})),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json());
        let post = app.get(&format!("/api/v1/posts/{id}"), &t).await.json();
        assert_eq!(
            post["figures"][0]["tags"]["hair"],
            json!(["blonde_hair", "long_hair"])
        );
        // An alias given when saving is followed too.
        let r = put_figures(
            &app,
            &t,
            id,
            json!([{"kind": "girl", "tags": {"hair": ["blond_hair", "blonde_hair"]}}]),
        )
        .await;
        assert_eq!(
            r.json()["figures"][0]["tags"]["hair"],
            json!(["blonde_hair"])
        );
        assert_eq!(sorted_tags(&r.json()), ["1girl", "blonde_hair", "solo"]);
        app.finish().await;
    }
}
