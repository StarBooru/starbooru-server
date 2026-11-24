//! Profiles: a display name, a short text about the account and a picture,
//! with what the account has done. Others see them on a public instance; the
//! username stays the handle.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::Role;

pub const MAX_DISPLAY_NAME_CHARS: usize = 40;
pub const MAX_ABOUT_CHARS: usize = 500;
/// Largest picture `PUT /auth/avatar` takes.
pub const MAX_AVATAR_BYTES: usize = 5 << 20;
/// Sides of the two copies kept of a picture, in pixels.
pub const AVATAR_SIZE: u32 = 256;
pub const AVATAR_SMALL_SIZE: u32 = 64;
/// The smallest square a picture may be cut to.
pub const MIN_AVATAR_SIDE: u32 = 16;

/// Who wrote or uploaded something, as others see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserCard {
    /// The username.
    pub name: String,
    #[serde(default)]
    pub display_name: Option<String>,
    /// Signed, server-relative URL of the 256 px picture. `None` without
    /// one: the apps draw an initial.
    #[serde(default)]
    pub avatar_url: Option<String>,
    /// The same at 64 px.
    #[serde(default)]
    pub avatar_small_url: Option<String>,
}

/// `GET /users/{name}` and `GET /auth/profile`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub display_name: Option<String>,
    pub about: Option<String>,
    pub avatar_url: Option<String>,
    pub avatar_small_url: Option<String>,
    pub role: Role,
    pub joined: DateTime<Utc>,
    /// Posts uploaded, outside the recycle bin.
    pub posts: u64,
    /// Forum posts written, deleted ones left out.
    pub forum_posts: u64,
}

/// `PATCH /auth/profile`: a field left out stays as it is; `null` or `""`
/// clears it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileUpdate {
    #[serde(
        default,
        deserialize_with = "crate::admin::present",
        skip_serializing_if = "Option::is_none"
    )]
    pub display_name: Option<Option<String>>,
    #[serde(
        default,
        deserialize_with = "crate::admin::present",
        skip_serializing_if = "Option::is_none"
    )]
    pub about: Option<Option<String>>,
}

/// The square a profile picture is cut from, in pixels of the upright
/// picture: the query of `PUT /auth/avatar?x=&y=&size=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvatarCrop {
    pub x: u32,
    pub y: u32,
    pub size: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProfileError {
    #[error("the display name is longer than {MAX_DISPLAY_NAME_CHARS} characters")]
    NameTooLong,
    #[error("the display name contains a control or invisible character")]
    NameControlChar,
    #[error("the text about you is longer than {MAX_ABOUT_CHARS} characters")]
    AboutTooLong,
    #[error("the text about you contains a control character")]
    AboutControlChar,
    #[error(
        "the picture is too small: it needs at least {MIN_AVATAR_SIDE} by {MIN_AVATAR_SIDE} pixels"
    )]
    PictureTooSmall,
    #[error("the square to cut goes outside the picture")]
    CropOutside,
}

/// Characters that hide or reorder text, so that one name could pass for
/// another.
pub(crate) fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

/// Whitespace runs as one space, trimmed.
pub(crate) fn one_line(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Line breaks as `\n`, trimmed at both ends.
pub(crate) fn lines(raw: &str) -> String {
    raw.replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim()
        .to_owned()
}

/// A control character other than a line break or a tab.
pub(crate) fn has_control(text: &str) -> bool {
    text.chars()
        .any(|c| (c.is_control() && c != '\n' && c != '\t') || is_invisible(c))
}

/// Trims a display name and collapses its spaces; empty means none.
pub fn clean_display_name(raw: &str) -> Result<Option<String>, ProfileError> {
    let name = one_line(raw);
    if name.is_empty() {
        return Ok(None);
    }
    if name.chars().count() > MAX_DISPLAY_NAME_CHARS {
        return Err(ProfileError::NameTooLong);
    }
    if name.chars().any(|c| c.is_control() || is_invisible(c)) {
        return Err(ProfileError::NameControlChar);
    }
    Ok(Some(name))
}

/// Trims the text about an account, keeping its lines; empty means none.
pub fn clean_about(raw: &str) -> Result<Option<String>, ProfileError> {
    let text = lines(raw);
    if text.is_empty() {
        return Ok(None);
    }
    if text.chars().count() > MAX_ABOUT_CHARS {
        return Err(ProfileError::AboutTooLong);
    }
    if has_control(&text) {
        return Err(ProfileError::AboutControlChar);
    }
    Ok(Some(text))
}

/// The square to cut from a `width` × `height` picture: `crop` when it fits
/// inside, or the largest one in the middle.
pub fn avatar_square(
    width: u32,
    height: u32,
    crop: Option<AvatarCrop>,
) -> Result<AvatarCrop, ProfileError> {
    let side = width.min(height);
    if side < MIN_AVATAR_SIDE {
        return Err(ProfileError::PictureTooSmall);
    }
    let Some(crop) = crop else {
        return Ok(AvatarCrop {
            x: width.saturating_sub(side) / 2,
            y: height.saturating_sub(side) / 2,
            size: side,
        });
    };
    if crop.size < MIN_AVATAR_SIDE {
        return Err(ProfileError::PictureTooSmall);
    }
    let inside = |at: u32, room: u32| at.checked_add(crop.size).is_some_and(|end| end <= room);
    if inside(crop.x, width) && inside(crop.y, height) {
        Ok(crop)
    } else {
        Err(ProfileError::CropOutside)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn display_names() {
        assert_eq!(
            clean_display_name("  Star   fan "),
            Ok(Some("Star fan".into()))
        );
        assert_eq!(clean_display_name("   "), Ok(None));
        assert_eq!(clean_display_name("a\nb"), Ok(Some("a b".into())));
        assert_eq!(
            clean_display_name(&"é".repeat(MAX_DISPLAY_NAME_CHARS)),
            Ok(Some("é".repeat(MAX_DISPLAY_NAME_CHARS)))
        );
        assert_eq!(
            clean_display_name(&"x".repeat(MAX_DISPLAY_NAME_CHARS + 1)),
            Err(ProfileError::NameTooLong)
        );
        for bad in ["a\u{0}b", "ad\u{202E}min", "a\u{200B}b", "\u{7f}"] {
            assert_eq!(
                clean_display_name(bad),
                Err(ProfileError::NameControlChar),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn about_keeps_its_lines() {
        assert_eq!(
            clean_about("\r\n Hi!\r\nI tag cats.\t \n"),
            Ok(Some("Hi!\nI tag cats.".into()))
        );
        assert_eq!(clean_about(" \n "), Ok(None));
        assert!(clean_about(&"x".repeat(MAX_ABOUT_CHARS)).is_ok());
        assert_eq!(
            clean_about(&"x".repeat(MAX_ABOUT_CHARS + 1)),
            Err(ProfileError::AboutTooLong)
        );
        assert_eq!(
            clean_about("a\u{1b}[31m"),
            Err(ProfileError::AboutControlChar)
        );
    }

    #[test]
    fn squares_cut_from_pictures() {
        let sq = |x, y, size| AvatarCrop { x, y, size };
        // Centred by default.
        assert_eq!(avatar_square(300, 100, None), Ok(sq(100, 0, 100)));
        assert_eq!(avatar_square(100, 301, None), Ok(sq(0, 100, 100)));
        assert_eq!(avatar_square(64, 64, None), Ok(sq(0, 0, 64)));
        // The client's square, when it fits.
        assert_eq!(
            avatar_square(300, 100, Some(sq(200, 0, 100))),
            Ok(sq(200, 0, 100))
        );
        assert_eq!(
            avatar_square(300, 100, Some(sq(201, 0, 100))),
            Err(ProfileError::CropOutside)
        );
        assert_eq!(
            avatar_square(300, 100, Some(sq(u32::MAX, 0, 100))),
            Err(ProfileError::CropOutside)
        );
        assert_eq!(
            avatar_square(300, 100, Some(sq(0, 0, 8))),
            Err(ProfileError::PictureTooSmall)
        );
        assert_eq!(
            avatar_square(500, 10, None),
            Err(ProfileError::PictureTooSmall)
        );
        assert_eq!(
            avatar_square(0, 0, None),
            Err(ProfileError::PictureTooSmall)
        );
    }

    #[test]
    fn an_update_tells_null_from_missing() {
        let parse = |v: serde_json::Value| serde_json::from_value::<ProfileUpdate>(v).unwrap();
        assert_eq!(parse(json!({})), ProfileUpdate::default());
        assert_eq!(parse(json!({ "about": null })).about, Some(None));
        assert_eq!(
            parse(json!({ "display_name": "Ann" })).display_name,
            Some(Some("Ann".into()))
        );
        let out = serde_json::to_value(ProfileUpdate {
            about: Some(None),
            ..ProfileUpdate::default()
        })
        .unwrap();
        assert_eq!(out, json!({ "about": null }));
    }
}
