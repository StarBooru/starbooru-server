//! Crops of the faces of figures. A figure with a face box gets a small
//! square JPEG of it, cut from the original (or the frame a video's
//! thumbnail comes from) by a background task, so saving people never
//! waits for it. Crops are named by their SHA-256, kept in `figures.face_sha`,
//! and removed once no figure names them.

use std::collections::HashSet;
use std::time::Duration;

use chrono::Utc;
use image::DynamicImage;
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};
use starbooru_core::{FACE_SCALE, FACE_SIZE, FaceBox, MediaKind};
use tracing::{info, warn};

use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::storage::{self, Storage, Variant};

/// Room left around a face on each side, as a share of the box's longer
/// side, so the crop shows the head and not only the features.
const MARGIN: f64 = 0.25;
/// Taken while crop files are written or removed, so one figure doesn't
/// lose a file another has just taken up.
const FACE_LOCK: i64 = 7279;
/// Posts cropped per round of the background task.
const BATCH: i64 = 10;
/// How often the background task looks without being woken.
const IDLE: Duration = Duration::from_secs(3600);

/// The square to cut for `face` from a `width` × `height` picture, as
/// `(x, y, side)` in pixels: around the box's middle, a margin larger, and
/// moved or shrunk to stay inside. `None` for an empty picture.
pub fn square(width: u32, height: u32, face: FaceBox) -> Option<(u32, u32, u32)> {
    if width == 0 || height == 0 {
        return None;
    }
    let (w, h) = (f64::from(width), f64::from(height));
    let scale = f64::from(FACE_SCALE);
    let (bw, bh) = (f64::from(face.w) / scale * w, f64::from(face.h) / scale * h);
    let cx = f64::from(face.x) / scale * w + bw / 2.0;
    let cy = f64::from(face.y) / scale * h + bh / 2.0;
    let fits = width.min(height);
    let side = (bw.max(bh) * (1.0 + 2.0 * MARGIN)).round().max(1.0);
    let side = (side as u32).clamp(1, fits);
    let half = f64::from(side) / 2.0;
    let x = ((cx - half).round().max(0.0) as u32).min(width - side);
    let y = ((cy - half).round().max(0.0) as u32).min(height - side);
    Some((x, y, side))
}

/// The crop of `face` from `img`, a `FACE_SIZE` square JPEG. CPU-bound.
pub fn cut(img: &DynamicImage, face: FaceBox) -> AppResult<Vec<u8>> {
    let (x, y, side) = square(img.width(), img.height(), face)
        .ok_or_else(|| AppError::BadRequest("the picture is empty".into()))?;
    storage::square_jpeg(&img.crop_imm(x, y, side, side), FACE_SIZE)
}

/// The signed URL of a crop.
pub fn url(state: &AppState, sha_hex: &str) -> String {
    state.signer.url(
        Variant::Face,
        &Storage::file_name(Variant::Face, sha_hex, ""),
        Utc::now().timestamp(),
    )
}

async fn lock(tx: &mut Transaction<'_, Postgres>) -> AppResult<()> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(FACE_LOCK)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// A figure whose face is waiting for its crop.
type Due = (i64, i16, i16, i16, i16);

fn face_of((_, x, y, w, h): Due) -> Option<FaceBox> {
    let edge = |v: i16| u16::try_from(v).ok();
    let face = FaceBox {
        x: edge(x)?,
        y: edge(y)?,
        w: edge(w)?,
        h: edge(h)?,
    };
    face.is_valid().then_some(face)
}

/// Makes the crops every figure with a face box is waiting for, a post at
/// a time, so each original is read once. A post whose picture can't be
/// read has its figures marked, and they wait for the next start. Returns
/// how many crops it made.
pub async fn make_due(state: &AppState) -> AppResult<u64> {
    let mut made = 0u64;
    loop {
        let posts: Vec<(i64, Vec<u8>, String, i16)> = sqlx::query_as(
            "SELECT p.id, p.sha256, p.ext, p.kind FROM posts p WHERE p.id IN ( \
                 SELECT DISTINCT post_id FROM figures \
                 WHERE face_x IS NOT NULL AND face_sha IS NULL AND NOT face_failed \
                 ORDER BY post_id LIMIT $1) \
             ORDER BY p.id",
        )
        .bind(BATCH)
        .fetch_all(&state.db)
        .await?;
        if posts.is_empty() {
            if made > 0 {
                state.franchises.notify_one();
            }
            return Ok(made);
        }
        for (post_id, sha, ext, kind) in posts {
            made =
                made.saturating_add(crop_post(state, post_id, &hex::encode(sha), ext, kind).await?);
        }
    }
}

async fn crop_post(
    state: &AppState,
    post_id: i64,
    sha_hex: &str,
    ext: String,
    kind: i16,
) -> AppResult<u64> {
    let due: Vec<Due> = sqlx::query_as(
        "SELECT id, face_x, face_y, face_w, face_h FROM figures \
         WHERE post_id = $1 AND face_x IS NOT NULL AND face_sha IS NULL AND NOT face_failed",
    )
    .bind(post_id)
    .fetch_all(&state.db)
    .await?;
    let faces: Vec<(i64, Option<FaceBox>)> = due.iter().map(|d| (d.0, face_of(*d))).collect();
    let kind = MediaKind::from_db(kind).unwrap_or(MediaKind::Image);
    let (st, name) = (state.clone(), sha_hex.to_owned());
    let boxes = faces.clone();
    let crops = tokio::task::spawn_blocking(move || {
        // A picture that can't be read, even for want of permission, fails
        // this post's faces only: the next posts still get theirs.
        let img = st.storage.still(&name, &ext, kind).unwrap_or_else(|e| {
            warn!("post {post_id}: its picture can't be read for face crops: {e}");
            None
        });
        let Some(img) = img else {
            return boxes
                .into_iter()
                .map(|(id, _)| (id, None))
                .collect::<Vec<_>>();
        };
        boxes
            .into_iter()
            .map(|(id, face)| (id, face.and_then(|f| cut(&img, f).ok())))
            .collect()
    })
    .await?;

    let mut made = 0u64;
    let mut failed = Vec::new();
    for (figure_id, jpeg) in crops {
        let Some(jpeg) = jpeg else {
            failed.push(figure_id);
            continue;
        };
        let face = faces
            .iter()
            .find(|(id, _)| *id == figure_id)
            .and_then(|(_, f)| *f);
        let Some(face) = face else {
            failed.push(figure_id);
            continue;
        };
        if keep(state, figure_id, face, &jpeg).await? {
            made = made.saturating_add(1);
        }
    }
    if !failed.is_empty() {
        warn!(
            "post {post_id}: {} face crop(s) couldn't be made; trying again at the next start",
            failed.len()
        );
        sqlx::query("UPDATE figures SET face_failed = true WHERE id = ANY($1)")
            .bind(&failed)
            .execute(&state.db)
            .await?;
    }
    Ok(made)
}

/// Stores a crop and names it on its figure, if the figure still has that
/// box; otherwise the file goes again. Whether it was kept.
async fn keep(state: &AppState, figure_id: i64, face: FaceBox, jpeg: &[u8]) -> AppResult<bool> {
    let sha: [u8; 32] = Sha256::digest(jpeg).into();
    let mut tx = state.db.begin().await?;
    lock(&mut tx).await?;
    let (st, name, bytes) = (state.clone(), hex::encode(sha), jpeg.to_vec());
    tokio::task::spawn_blocking(move || st.storage.write_face(&name, &bytes)).await??;
    let kept = sqlx::query(
        "UPDATE figures SET face_sha = $2 WHERE id = $1 AND face_sha IS NULL \
         AND face_x = $3 AND face_y = $4 AND face_w = $5 AND face_h = $6",
    )
    .bind(figure_id)
    .bind(sha.as_slice())
    .bind(i16::try_from(face.x).unwrap_or(i16::MAX))
    .bind(i16::try_from(face.y).unwrap_or(i16::MAX))
    .bind(i16::try_from(face.w).unwrap_or(i16::MAX))
    .bind(i16::try_from(face.h).unwrap_or(i16::MAX))
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    if kept {
        // The character may show this face now.
        sqlx::query(
            "UPDATE tags SET franchise_checked_at = NULL WHERE id IN \
             (SELECT tag_id FROM figure_tags WHERE figure_id = $1 AND slot = 0)",
        )
        .bind(figure_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    if !kept {
        release(state, vec![sha.to_vec()]).await?;
    }
    Ok(kept)
}

/// Removes the crops among `shas` that no figure names any more.
pub async fn release(state: &AppState, shas: Vec<Vec<u8>>) -> AppResult<()> {
    if shas.is_empty() {
        return Ok(());
    }
    let mut tx = state.db.begin().await?;
    lock(&mut tx).await?;
    let used: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT DISTINCT face_sha FROM figures WHERE face_sha = ANY($1)")
            .bind(&shas)
            .fetch_all(&mut *tx)
            .await?;
    let gone: Vec<String> = shas
        .into_iter()
        .filter(|s| !used.contains(s))
        .map(hex::encode)
        .collect();
    if !gone.is_empty() {
        let st = state.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            for name in gone {
                st.storage.remove_face(&name)?;
            }
            Ok(())
        })
        .await??;
    }
    tx.commit().await?;
    Ok(())
}

/// Removes the crop files no figure names: what a crash between writing a
/// file and naming it left behind. Returns how many.
pub async fn sweep(state: &AppState) -> AppResult<u64> {
    let mut tx = state.db.begin().await?;
    lock(&mut tx).await?;
    let used: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT DISTINCT face_sha FROM figures WHERE face_sha IS NOT NULL")
            .fetch_all(&mut *tx)
            .await?;
    let used: HashSet<String> = used.into_iter().map(hex::encode).collect();
    let st = state.clone();
    let removed = tokio::task::spawn_blocking(move || -> std::io::Result<u64> {
        let mut removed = 0u64;
        for name in st.storage.face_names()? {
            if !used.contains(&name) {
                st.storage.remove_face(&name)?;
                removed = removed.saturating_add(1);
            }
        }
        Ok(removed)
    })
    .await??;
    tx.commit().await?;
    Ok(removed)
}

/// Forgets the crops whose files are missing, as after restoring a
/// database backup without the data folder it went with, so they are made
/// again. Returns how many.
pub async fn forget_missing(state: &AppState) -> AppResult<u64> {
    let mut after: Vec<u8> = Vec::new();
    let mut forgotten = 0u64;
    loop {
        let shas: Vec<Vec<u8>> = sqlx::query_scalar(
            "SELECT DISTINCT face_sha FROM figures WHERE face_sha > $1 ORDER BY face_sha LIMIT 1000",
        )
        .bind(&after)
        .fetch_all(&state.db)
        .await?;
        let Some(last) = shas.last().cloned() else {
            return Ok(forgotten);
        };
        after = last;
        let st = state.clone();
        let missing: Vec<Vec<u8>> = tokio::task::spawn_blocking(move || {
            shas.into_iter()
                .filter(|s| !st.storage.has_face(&hex::encode(s)))
                .collect()
        })
        .await?;
        if !missing.is_empty() {
            let n = sqlx::query("UPDATE figures SET face_sha = NULL WHERE face_sha = ANY($1)")
                .bind(&missing)
                .execute(&state.db)
                .await?
                .rows_affected();
            forgotten = forgotten.saturating_add(n);
        }
    }
}

/// The background task: at start, retries crops that failed before, makes
/// again those whose files are missing and removes files nothing names;
/// then makes crops whenever saved people are waiting for them.
pub async fn run(state: AppState) {
    let start = async {
        sqlx::query("UPDATE figures SET face_failed = false WHERE face_failed")
            .execute(&state.db)
            .await?;
        let missing = forget_missing(&state).await?;
        if missing > 0 {
            info!("{missing} face crop(s) were missing and will be made again");
        }
        let removed = sweep(&state).await?;
        if removed > 0 {
            info!("removed {removed} face crop(s) no figure uses");
        }
        AppResult::Ok(())
    };
    if let Err(e) = start.await {
        warn!("face crops: {e}");
    }
    loop {
        match make_due(&state).await {
            Ok(0) => {}
            Ok(n) => info!("made {n} face crop(s)"),
            Err(e) => warn!("face crops: {e}"),
        }
        let _ = tokio::time::timeout(IDLE, state.faces.notified()).await;
    }
}

#[cfg(test)]
mod tests {
    use image::{Rgb, RgbImage};

    use super::*;

    fn at(x: u16, y: u16, w: u16, h: u16) -> FaceBox {
        FaceBox { x, y, w, h }
    }

    #[test]
    fn squares_stay_around_the_face_and_inside() {
        // 100 × 50 px in the middle of 1000 × 500: 150 px with the margin.
        assert_eq!(
            square(1000, 500, at(450, 450, 100, 100)),
            Some((425, 175, 150))
        );
        // At the top-left corner it is moved in, not cut.
        assert_eq!(square(1000, 500, at(0, 0, 100, 100)), Some((0, 0, 150)));
        assert_eq!(
            square(1000, 500, at(900, 900, 100, 100)),
            Some((850, 350, 150))
        );
        // Never larger than the picture's shorter side.
        assert_eq!(square(1000, 500, at(0, 0, 1000, 1000)), Some((250, 0, 500)));
        // A tall box: the square follows the longer side.
        assert_eq!(
            square(1000, 1000, at(500, 100, 10, 200)),
            Some((355, 50, 300))
        );
        // Tiny boxes and pictures still give at least a pixel.
        assert_eq!(square(1, 1, at(0, 0, 1, 1)), Some((0, 0, 1)));
        assert_eq!(square(7, 3000, at(999, 999, 1, 1)), Some((2, 2995, 5)));
        assert_eq!(square(0, 10, at(0, 0, 10, 10)), None);
        for (w, h) in [(1, 1), (3, 9000), (9000, 3), (640, 480)] {
            for face in [
                at(0, 0, 1000, 1000),
                at(999, 0, 1, 1000),
                at(500, 500, 1, 1),
            ] {
                let (x, y, side) = square(w, h, face).unwrap();
                assert!(
                    side >= 1 && x + side <= w && y + side <= h,
                    "{w}×{h} {face:?}"
                );
            }
        }
    }

    #[test]
    fn cuts_the_face_out() {
        // Blue on the right half, a red square in its middle.
        let img = DynamicImage::ImageRgb8(RgbImage::from_fn(400, 200, |x, y| {
            if (250..350).contains(&x) && (50..150).contains(&y) {
                Rgb([230, 20, 20])
            } else if x >= 200 {
                Rgb([20, 20, 230])
            } else {
                Rgb([240, 240, 240])
            }
        }));
        let jpeg = cut(&img, at(625, 250, 250, 500)).unwrap();
        let crop = storage::decode(&jpeg).unwrap().to_rgb8();
        assert_eq!(crop.dimensions(), (FACE_SIZE, FACE_SIZE));
        // Red in the middle, blue in the margin, no grey from the left half.
        let middle = crop.get_pixel(FACE_SIZE / 2, FACE_SIZE / 2).0;
        assert!(middle[0] > 180 && middle[2] < 80, "{middle:?}");
        let corner = crop.get_pixel(3, 3).0;
        assert!(corner[2] > 180 && corner[0] < 80, "{corner:?}");
    }
}
