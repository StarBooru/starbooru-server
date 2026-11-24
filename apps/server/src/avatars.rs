//! Profile pictures. An upload is cut to a square and kept as two small
//! JPEGs named by the SHA-256 of the larger; the upload itself isn't kept.
//! The files go once no account uses them.

use std::collections::HashSet;

use chrono::Utc;
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};
use starbooru_core::profile::{AVATAR_SIZE, AVATAR_SMALL_SIZE};
use starbooru_core::{AvatarCrop, UserCard, avatar_square, detect_mime};

use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::storage::{self, Storage, Variant};

const ACCEPTED: [&str; 4] = ["image/jpeg", "image/png", "image/webp", "image/gif"];
/// A picture whose pixels would take more memory than this isn't decoded.
const MAX_DECODE_BYTES: u64 = 256 << 20;
/// Taken while a picture's files are written or removed, so one account
/// doesn't lose files another has just taken up.
const AVATAR_LOCK: i64 = 7278;

/// A picture ready to store.
pub struct Made {
    pub sha: [u8; 32],
    big: Vec<u8>,
    small: Vec<u8>,
}

/// Cuts an upload to `crop` (or the middle) and scales it to both sizes. A
/// GIF gives its first frame. CPU-bound: call from a blocking thread.
pub fn make(bytes: &[u8], crop: Option<AvatarCrop>) -> AppResult<Made> {
    detect_mime(bytes)
        .filter(|mime| ACCEPTED.contains(mime))
        .ok_or_else(|| {
            AppError::Unsupported("a profile picture is a JPEG, PNG, WebP or GIF".into())
        })?;
    let img = storage::decode_within(bytes, MAX_DECODE_BYTES)?;
    let square = avatar_square(img.width(), img.height(), crop)?;
    let img = img.crop_imm(square.x, square.y, square.size, square.size);
    let big = storage::square_jpeg(&img, AVATAR_SIZE)?;
    let small = storage::square_jpeg(&img, AVATAR_SMALL_SIZE)?;
    Ok(Made {
        sha: Sha256::digest(&big).into(),
        big,
        small,
    })
}

async fn lock(tx: &mut Transaction<'_, Postgres>) -> AppResult<()> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(AVATAR_LOCK)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Makes `made` the account's picture, on a public instance (`off` is the
/// refusal's code and message otherwise).
pub async fn set(
    state: &AppState,
    user_id: i64,
    made: Made,
    off: (&'static str, &'static str),
) -> AppResult<()> {
    let mut tx = state.db.begin().await?;
    // Held public until saved; a switch to private then removes it.
    crate::modes::hold_public(&mut tx, off.0, off.1).await?;
    lock(&mut tx).await?;
    let old: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT avatar_sha FROM users WHERE id = $1 FOR UPDATE")
            .bind(user_id)
            .fetch_one(&mut *tx)
            .await?;
    let st = state.clone();
    let name = hex::encode(made.sha);
    tokio::task::spawn_blocking(move || st.storage.write_avatar(&name, &made.big, &made.small))
        .await??;
    sqlx::query("UPDATE users SET avatar_sha = $2 WHERE id = $1")
        .bind(user_id)
        .bind(made.sha.as_slice())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    release(state, old).await
}

/// Takes the account's picture away, if it has one.
pub async fn clear(state: &AppState, user_id: i64) -> AppResult<()> {
    let mut tx = state.db.begin().await?;
    let old: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT avatar_sha FROM users WHERE id = $1 FOR UPDATE")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?
            .flatten();
    if old.is_none() {
        return Ok(());
    }
    sqlx::query("UPDATE users SET avatar_sha = NULL WHERE id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    release(state, old).await
}

/// Removes a picture's files once no account has it.
async fn release(state: &AppState, sha: Option<Vec<u8>>) -> AppResult<()> {
    let Some(sha) = sha else {
        return Ok(());
    };
    let mut tx = state.db.begin().await?;
    lock(&mut tx).await?;
    let used: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE avatar_sha = $1)")
            .bind(&sha)
            .fetch_one(&mut *tx)
            .await?;
    if !used {
        let st = state.clone();
        tokio::task::spawn_blocking(move || st.storage.remove_avatar(&hex::encode(sha))).await??;
    }
    tx.commit().await?;
    Ok(())
}

/// Removes the picture files no account uses: after a switch to private,
/// and at start for any that a crash left behind. Returns how many.
pub async fn sweep(state: &AppState) -> AppResult<u64> {
    let mut tx = state.db.begin().await?;
    lock(&mut tx).await?;
    let used: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT DISTINCT avatar_sha FROM users WHERE avatar_sha IS NOT NULL")
            .fetch_all(&mut *tx)
            .await?;
    let used: HashSet<String> = used.into_iter().map(hex::encode).collect();
    let st = state.clone();
    let removed = tokio::task::spawn_blocking(move || -> std::io::Result<u64> {
        let mut removed = 0u64;
        for name in st.storage.avatar_names()? {
            if !used.contains(&name) {
                st.storage.remove_avatar(&name)?;
                removed = removed.saturating_add(1);
            }
        }
        Ok(removed)
    })
    .await??;
    tx.commit().await?;
    Ok(removed)
}

/// The signed URLs of a picture: 256 px, then 64 px.
pub fn urls(state: &AppState, sha_hex: &str) -> (String, String) {
    let now = Utc::now().timestamp();
    let url = |variant| {
        state
            .signer
            .url(variant, &Storage::file_name(variant, sha_hex, ""), now)
    };
    (url(Variant::Avatar), url(Variant::AvatarSmall))
}

/// How others see an account next to what it wrote or uploaded.
pub fn card(
    state: &AppState,
    name: String,
    display_name: Option<String>,
    avatar_hex: Option<&str>,
) -> UserCard {
    let (avatar_url, avatar_small_url) = avatar_hex.map(|sha| urls(state, sha)).unzip();
    UserCard {
        name,
        display_name,
        avatar_url,
        avatar_small_url,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn encoded(img: image::DynamicImage, format: image::ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    /// Left half red, right half blue, `w` × `h`.
    fn halves(w: u32, h: u32) -> image::DynamicImage {
        image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(w, h, |x, _| {
            if x < w / 2 {
                image::Rgb([230, 20, 20])
            } else {
                image::Rgb([20, 20, 230])
            }
        }))
    }

    #[test]
    fn cuts_and_scales_to_two_squares() {
        for format in [
            image::ImageFormat::Png,
            image::ImageFormat::Jpeg,
            image::ImageFormat::WebP,
            image::ImageFormat::Gif,
        ] {
            let made = make(&encoded(halves(600, 300), format), None).unwrap();
            let big = storage::decode(&made.big).unwrap();
            let small = storage::decode(&made.small).unwrap();
            assert_eq!((big.width(), big.height()), (AVATAR_SIZE, AVATAR_SIZE));
            assert_eq!((small.width(), small.height()), (64, 64));
            assert_eq!(made.sha, <[u8; 32]>::from(Sha256::digest(&made.big)));
        }
        // The middle by default: half red, half blue.
        let png = encoded(halves(600, 300), image::ImageFormat::Png);
        let mid = storage::decode(&make(&png, None).unwrap().big)
            .unwrap()
            .to_rgb8();
        assert!(mid.get_pixel(10, 128).0[0] > 200);
        assert!(mid.get_pixel(245, 128).0[2] > 200);
        // The client's square: all blue.
        let crop = AvatarCrop {
            x: 400,
            y: 50,
            size: 200,
        };
        let right = storage::decode(&make(&png, Some(crop)).unwrap().big)
            .unwrap()
            .to_rgb8();
        assert!(right.pixels().all(|p| p.0[2] > 200 && p.0[0] < 60));
        // A tiny picture is scaled up, so every copy has the same size.
        let tiny = make(&encoded(halves(20, 20), image::ImageFormat::Png), None).unwrap();
        assert_eq!(storage::decode(&tiny.big).unwrap().width(), AVATAR_SIZE);
    }

    #[test]
    fn refuses_what_isnt_a_picture_for_a_profile() {
        let bmp = encoded(halves(40, 40), image::ImageFormat::Bmp);
        assert!(matches!(make(&bmp, None), Err(AppError::Unsupported(_))));
        assert!(matches!(
            make(b"not a picture", None),
            Err(AppError::Unsupported(_))
        ));
        let png = encoded(halves(40, 40), image::ImageFormat::Png);
        assert!(matches!(
            make(&png[..png.len() / 2], None),
            Err(AppError::BadRequest(_))
        ));
        let small = encoded(halves(12, 40), image::ImageFormat::Png);
        assert!(matches!(make(&small, None), Err(AppError::BadRequest(_))));
        let outside = AvatarCrop {
            x: 30,
            y: 0,
            size: 20,
        };
        assert!(matches!(
            make(&png, Some(outside)),
            Err(AppError::BadRequest(_))
        ));
    }
}
