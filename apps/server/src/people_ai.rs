//! The people tool: finds the people in a post's picture and fills in a
//! figure for each, with its face box (`faces.rs` then keeps the crop).
//!
//! The built-in models find the faces and read each person with a tagger
//! (`starbooru_core::people_ai`). An AI provider gets the picture with the
//! faces found here, or finds them itself when the face finders can't run.
//! Never log pictures or what was found in them.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use base64::Engine as _;
use image::DynamicImage;
use image::imageops::FilterType;
use starbooru_core::figure::{Figure, MAX_FIGURES, clean_figures};
use starbooru_core::people_ai::{self, FoundPeople};
use starbooru_core::{AiMode, AiTool, FaceBox, MediaKind, TagCategory};
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::ai::{self, Route};
use crate::error::{AppError, AppResult};
use crate::local_ai;
use crate::state::AppState;
use crate::storage::{self, FilterKind};

/// Longest side of the picture the models see: faces in a group photo
/// stay large enough to find.
const PICTURE_SIZE: u32 = 1536;
/// One picture at a time on upload: each runs the tagger a few times.
static SLOTS: Semaphore = Semaphore::const_new(1);
/// How deep the tag tree is looked up.
const MAX_DEPTH: i32 = 32;

/// A post's still picture, fit in [`PICTURE_SIZE`]: as the models see it
/// and decoded, for previews of the crops.
struct Picture {
    jpeg: Vec<u8>,
    image: DynamicImage,
}

async fn picture(state: &AppState, post_id: i64) -> AppResult<Picture> {
    let row: Option<(Vec<u8>, String, i16)> =
        sqlx::query_as("SELECT sha256, ext, kind FROM posts WHERE id = $1 AND deleted_at IS NULL")
            .bind(post_id)
            .fetch_optional(&state.db)
            .await?;
    let (sha, ext, kind) = row.ok_or(AppError::NotFound)?;
    let kind = MediaKind::from_db(kind).unwrap_or(MediaKind::Image);
    let st = state.clone();
    tokio::task::spawn_blocking(move || {
        // The frame a video's thumbnail comes from, as the crops are cut from.
        let still = st.storage.still(&hex::encode(sha), &ext, kind)?;
        let img = still.ok_or_else(|| {
            AppError::BadRequest("this post has no picture to find people in".into())
        })?;
        let image = if img.width().max(img.height()) > PICTURE_SIZE {
            img.resize(PICTURE_SIZE, PICTURE_SIZE, FilterType::CatmullRom)
        } else {
            img
        };
        let jpeg = storage::fit_jpeg(&image, PICTURE_SIZE, FilterKind::Sharp)?;
        Ok(Picture { jpeg, image })
    })
    .await
    .map_err(AppError::internal)?
}

/// Each name (through aliases) with the tags above it in the tag tree,
/// nearest first. Names the collection doesn't have, or that have no
/// parent, aren't in it.
async fn tree(state: &AppState, names: &[String]) -> AppResult<HashMap<String, Vec<String>>> {
    let rows: Vec<(String, Vec<String>)> = sqlx::query_as(
        "WITH RECURSIVE start(name, id) AS ( \
             SELECT n.name, COALESCE(a.tag_id, t.id) FROM unnest($1::text[]) AS n(name) \
             LEFT JOIN tag_aliases a ON a.alias = n.name \
             LEFT JOIN tags t ON t.name = n.name \
             WHERE COALESCE(a.tag_id, t.id) IS NOT NULL \
         ), up(name, id, depth) AS ( \
             SELECT s.name, t.parent_id, 1 FROM start s JOIN tags t ON t.id = s.id \
             WHERE t.parent_id IS NOT NULL \
             UNION ALL SELECT up.name, t.parent_id, up.depth + 1 FROM up JOIN tags t ON t.id = up.id \
             WHERE t.parent_id IS NOT NULL AND up.depth < $2 \
         ) SELECT up.name, array_agg(t.name ORDER BY up.depth) FROM up \
         JOIN tags t ON t.id = up.id GROUP BY up.name",
    )
    .bind(names)
    .bind(MAX_DEPTH)
    .fetch_all(&state.db)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Which of `names` (through aliases) the post already has.
async fn on_post(state: &AppState, post_id: i64, names: &[String]) -> AppResult<HashSet<String>> {
    let found: Vec<String> = sqlx::query_scalar(
        "SELECT n.name FROM unnest($2::text[]) AS n(name) \
         JOIN tags t ON t.id = COALESCE((SELECT tag_id FROM tag_aliases WHERE alias = n.name), \
             (SELECT id FROM tags WHERE name = n.name)) \
         JOIN posts p ON p.id = $1 AND t.id = ANY(p.tag_ids)",
    )
    .bind(post_id)
    .bind(names)
    .fetch_all(&state.db)
    .await?;
    Ok(found.into_iter().collect())
}

/// The built-in models' people, as figures with their tags in slots the
/// way the collection's tag tree sorts them.
async fn find_local(
    state: &AppState,
    active: &local_ai::Active,
    post_id: i64,
    jpeg: &[u8],
) -> AppResult<Vec<Figure>> {
    let people = local_ai::people(state, active, jpeg).await?;
    // What the tagging tool saw in the whole picture: whether it's a
    // photograph, and the characters the faces' crops name.
    let mut wanted: Vec<String> = vec!["realistic".into(), "photorealistic".into()];
    wanted.extend(
        people
            .iter()
            .flat_map(|p| &p.face_tags)
            .filter(|t| t.category == Some(TagCategory::Character))
            .map(|t| t.name.clone()),
    );
    let tagged = on_post(state, post_id, &wanted).await?;
    let mut names: Vec<String> = people
        .iter()
        .flat_map(|p| p.face_tags.iter().chain(&p.body_tags))
        .filter_map(|t| starbooru_core::normalize_tag(&t.name).ok())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    names.sort();
    let tree = tree(state, &names).await?;
    Ok(people_ai::figures_from(
        &people,
        |name| tagged.contains(name),
        |name| {
            let above = tree.get(name).map_or(&[][..], Vec::as_slice);
            people_ai::slot_of(name, above)
        },
    ))
}

/// An AI provider's people. The faces found here go with the picture; when
/// none are, nobody is asked.
async fn find_api(
    state: &AppState,
    provider: &ai::Provider,
    jpeg: &[u8],
) -> AppResult<Vec<Figure>> {
    let faces: Option<Vec<FaceBox>> = local_ai::faces(state, jpeg).await.map(|found| {
        found
            .into_iter()
            .map(|f| f.face)
            .take(MAX_FIGURES)
            .collect()
    });
    let faces = match faces {
        Some(faces) if faces.is_empty() => return Ok(Vec::new()),
        Some(faces) => faces,
        // The face finders can't run here: the provider finds the faces too.
        None => Vec::new(),
    };
    let answer = ai::chat(provider, &people_ai::people_prompt(&faces), Some(jpeg)).await?;
    Ok(people_ai::parse_people(&answer, &faces))
}

/// The franchise each figure comes from: what the collection knows of its
/// character, else the series its name hints at, under the name the
/// collection has it by when it does.
async fn fill_franchises(state: &AppState, figures: &mut [Figure]) -> AppResult<()> {
    let names: Vec<String> = figures
        .iter()
        .filter(|f| f.franchise.is_none())
        .filter_map(|f| f.cosplay.clone().or_else(|| f.character.clone()))
        .collect();
    if names.is_empty() {
        return Ok(());
    }
    let mut tx = state.db.begin().await?;
    let known = crate::routes::franchises_by_name(&mut tx, &names).await?;
    drop(tx);
    let wanted = people_ai::series_names(figures);
    let found: HashMap<String, String> = sqlx::query_as(
        "SELECT n.name, t.name FROM unnest($1::text[]) AS n(name) \
         LEFT JOIN tag_aliases a ON a.alias = n.name \
         JOIN tags t ON t.id = COALESCE(a.tag_id, (SELECT id FROM tags WHERE name = n.name)) \
         WHERE t.category = $2",
    )
    .bind(&wanted)
    .bind(TagCategory::Copyright.to_db())
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .collect();
    // `fate` as it is, else `fate_(series)`.
    let series: HashMap<String, String> = wanted
        .iter()
        .filter(|n| !n.ends_with("_(series)"))
        .filter_map(|hint| {
            let tag = found
                .get(hint)
                .or_else(|| found.get(&format!("{hint}_(series)")))?;
            Some((hint.clone(), tag.clone()))
        })
        .collect();
    people_ai::franchises(figures, &known, &series);
    Ok(())
}

/// What finding people in a post gave.
struct Found {
    figures: Vec<Figure>,
    model: String,
    picture: Picture,
}

/// Finds the people in a post's picture. Changes nothing.
async fn find(state: &AppState, route: &Route, post_id: i64) -> AppResult<Found> {
    let picture = picture(state, post_id).await?;
    let (mut figures, model) = match route {
        Route::Local(active) => (
            find_local(state, active, post_id, &picture.jpeg).await?,
            active.model.name.to_owned(),
        ),
        Route::Api(provider) => (
            find_api(state, provider, &picture.jpeg).await?,
            provider.model.clone(),
        ),
    };
    fill_franchises(state, &mut figures).await?;
    // Read left to right, as people are named in a caption.
    figures.sort_by_key(|f| f.face.map_or(u16::MAX, |b| b.x));
    figures.truncate(MAX_FIGURES);
    let figures = clean_figures(figures).map_err(AppError::internal)?;
    Ok(Found {
        figures,
        model,
        picture,
    })
}

/// `POST /posts/{id}/find-people`: the people in a post's picture, each
/// with a preview of the crop saving it would keep. Nothing is saved.
pub async fn suggest(state: &AppState, route: &Route, post_id: i64) -> AppResult<FoundPeople> {
    let Found {
        mut figures,
        model,
        picture,
    } = find(state, route, post_id).await?;
    let boxes: Vec<Option<FaceBox>> = figures.iter().map(|f| f.face).collect();
    let crops = tokio::task::spawn_blocking(move || {
        boxes
            .into_iter()
            .map(|face| face.and_then(|f| crate::faces::cut(&picture.image, f).ok()))
            .collect::<Vec<_>>()
    })
    .await
    .map_err(AppError::internal)?;
    for (figure, crop) in figures.iter_mut().zip(crops) {
        figure.face_url = crop.map(|jpeg| {
            format!(
                "data:image/jpeg;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(jpeg)
            )
        });
    }
    Ok(FoundPeople { figures, model })
}

/// Whether the people tool runs on new posts.
pub async fn is_on(state: &AppState) -> bool {
    match crate::settings::ai(state).await {
        Ok(setup) => setup.tool(AiTool::People).mode != AiMode::Off,
        Err(e) => {
            warn!("the AI settings can't be read: {e}");
            false
        }
    }
}

/// Finds the people in a new post and saves them, when the tool is on and
/// the post has none yet. Whether it saved any.
pub async fn find_for_new_post(state: &AppState, post_id: i64) -> AppResult<bool> {
    let setup = crate::settings::ai(state).await?;
    let tool = setup.tool(AiTool::People);
    if tool.mode == AiMode::Off || has_figures(state, post_id).await? {
        return Ok(false);
    }
    let route = ai::route(state, &tool).await?;
    let _slot = SLOTS
        .acquire()
        .await
        .map_err(|_| AppError::Internal("finding people is shutting down".into()))?;
    let started = Instant::now();
    let found = find(state, &route, post_id).await?;
    let count = found.figures.len();
    let saved = ai::add_figures(state, post_id, found.figures).await?;
    info!(
        post_id,
        people = count,
        saved,
        ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "looked for people in a new post"
    );
    Ok(saved)
}

async fn has_figures(state: &AppState, post_id: i64) -> AppResult<bool> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM figures WHERE post_id = $1)")
            .bind(post_id)
            .fetch_one(&state.db)
            .await?,
    )
}
