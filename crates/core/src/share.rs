//! Share links: anyone with the link sees the chosen posts (media only, no
//! tags or account details) without signing in, until the link expires or
//! its owner removes it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model::MediaKind;

/// Links one account may keep.
pub const MAX_SHARES: usize = 500;
/// Posts in one link.
pub const MAX_SHARE_POSTS: usize = 500;
pub const MAX_SHARE_TITLE_CHARS: usize = 100;
/// Longest lifetime a link can be given; it may also never expire.
pub const MAX_SHARE_DAYS: u32 = 365;
/// Characters in a link's token (lowercase hex).
pub const SHARE_TOKEN_CHARS: usize = 32;

/// `POST /shares`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewShare {
    /// In the order they are shown.
    pub post_ids: Vec<i64>,
    #[serde(default)]
    pub title: Option<String>,
    /// Days until the link stops working; `None` keeps it until removed.
    #[serde(default)]
    pub days: Option<u32>,
    /// Whether chats and sites show the link with its first picture:
    /// `None` follows the account's [`SharingSettings::previews`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<bool>,
}

/// One of the caller's links.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Share {
    pub id: i64,
    pub token: String,
    pub title: Option<String>,
    /// Posts the link was made with (some may have gone to the bin since).
    pub posts: u32,
    /// Signed, server-relative thumbnail of the first post still shown.
    pub cover_url: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    /// Times the link was opened.
    pub views: i64,
    pub last_viewed_at: Option<DateTime<Utc>>,
    /// The link's own choice of preview; `None` follows the account's.
    #[serde(default)]
    pub preview: Option<bool>,
    /// Whether the link shows a preview now, after the server's and the
    /// account's settings ([`preview_on`]).
    #[serde(default)]
    pub preview_on: bool,
}

/// `PATCH /shares/{id}`: fields left out stay as they are.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareUpdate {
    /// `Some(None)` (JSON `null`) makes the link follow the account's
    /// setting again.
    #[serde(
        default,
        deserialize_with = "crate::admin::present",
        skip_serializing_if = "Option::is_none"
    )]
    pub preview: Option<Option<bool>>,
}

/// `GET` and `PUT /auth/sharing`: the account's defaults for its links.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharingSettings {
    /// New links, and those that don't choose, show a preview in chats.
    #[serde(default)]
    pub previews: bool,
}

/// Whether a share link shows its picture to chats and sites: the server
/// allows it (`link_previews` in the admin settings), and the link says yes
/// or leaves it to the account, which says yes.
pub fn preview_on(server: bool, account: bool, link: Option<bool>) -> bool {
    server && link.unwrap_or(account)
}

/// A post as a link shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedPost {
    pub kind: MediaKind,
    pub mime: String,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub duration_ms: Option<i64>,
    /// Signed, server-relative URL of the original file.
    pub file_url: String,
    pub thumb_url: Option<String>,
    /// A smaller copy for viewing, as on `Post`.
    #[serde(default)]
    pub sample_url: Option<String>,
}

/// `GET /shared/{token}`: what a link shows, no account needed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedView {
    pub title: Option<String>,
    /// The server's name.
    pub server: String,
    pub expires_at: Option<DateTime<Utc>>,
    /// Posts in the bin or erased since the link was made are left out.
    pub posts: Vec<SharedPost>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ShareError {
    #[error("pick at least one post to share")]
    NoPosts,
    #[error("a link can hold at most {MAX_SHARE_POSTS} posts")]
    TooManyPosts,
    #[error("the title is longer than {MAX_SHARE_TITLE_CHARS} characters")]
    TitleTooLong,
    #[error("the title contains a control character")]
    ControlChar,
    #[error("a link lasts 1 to {MAX_SHARE_DAYS} days, or until removed")]
    BadDays,
}

impl NewShare {
    /// Drops repeated posts (keeping the first place), trims the title
    /// (empty is none) and checks the limits.
    pub fn clean(self) -> Result<NewShare, ShareError> {
        let mut post_ids = Vec::with_capacity(self.post_ids.len().min(MAX_SHARE_POSTS));
        for id in self.post_ids {
            if !post_ids.contains(&id) {
                if post_ids.len() == MAX_SHARE_POSTS {
                    return Err(ShareError::TooManyPosts);
                }
                post_ids.push(id);
            }
        }
        if post_ids.is_empty() {
            return Err(ShareError::NoPosts);
        }
        let title = self
            .title
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty());
        if let Some(t) = &title {
            if t.chars().count() > MAX_SHARE_TITLE_CHARS {
                return Err(ShareError::TitleTooLong);
            }
            if t.chars().any(char::is_control) {
                return Err(ShareError::ControlChar);
            }
        }
        if self.days.is_some_and(|d| d == 0 || d > MAX_SHARE_DAYS) {
            return Err(ShareError::BadDays);
        }
        Ok(NewShare {
            post_ids,
            title,
            days: self.days,
            preview: self.preview,
        })
    }
}

/// The page a link opens, relative to the server (which serves the web app).
pub fn share_path(token: &str) -> String {
    format!("/s/{token}")
}

/// Whether `token` looks like a link token, before asking the database.
pub fn is_share_token(token: &str) -> bool {
    token.len() == SHARE_TOKEN_CHARS
        && token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn share(ids: Vec<i64>, title: Option<&str>, days: Option<u32>) -> NewShare {
        NewShare {
            post_ids: ids,
            title: title.map(str::to_owned),
            days,
            preview: None,
        }
    }

    #[test]
    fn previews_need_the_server_and_follow_the_account_unless_the_link_says() {
        assert!(!preview_on(false, true, Some(true)));
        assert!(preview_on(true, true, None));
        assert!(!preview_on(true, false, None));
        assert!(preview_on(true, false, Some(true)));
        assert!(!preview_on(true, true, Some(false)));
    }

    #[test]
    fn a_patch_tells_null_from_missing() {
        let missing: ShareUpdate = serde_json::from_str("{}").unwrap();
        assert_eq!(missing.preview, None);
        let follow: ShareUpdate = serde_json::from_str(r#"{"preview":null}"#).unwrap();
        assert_eq!(follow.preview, Some(None));
        let off: ShareUpdate = serde_json::from_str(r#"{"preview":false}"#).unwrap();
        assert_eq!(off.preview, Some(Some(false)));
        assert_eq!(
            serde_json::to_string(&ShareUpdate {
                preview: Some(None)
            })
            .unwrap(),
            r#"{"preview":null}"#
        );
        // An older server's link reads as following the account, without one.
        let old: Share = serde_json::from_str(
            r#"{"id":1,"token":"t","title":null,"posts":1,"cover_url":null,
                "created_at":"2026-11-26T00:00:00Z","expires_at":null,"views":0,
                "last_viewed_at":null}"#,
        )
        .unwrap();
        assert_eq!((old.preview, old.preview_on), (None, false));
    }

    #[test]
    fn cleans_new_shares() {
        let clean = share(vec![3, 1, 3, 2, 1], Some("  Trip  "), Some(7))
            .clean()
            .unwrap();
        assert_eq!(clean.post_ids, [3, 1, 2]);
        assert_eq!(clean.title.as_deref(), Some("Trip"));
        assert_eq!(clean.days, Some(7));
        assert_eq!(
            share(vec![1], Some("  "), None).clean().unwrap().title,
            None
        );
        assert_eq!(share(vec![], None, None).clean(), Err(ShareError::NoPosts));
        let many: Vec<i64> = (0..=500).collect();
        assert_eq!(
            share(many, None, None).clean(),
            Err(ShareError::TooManyPosts)
        );
        // Repeats don't count towards the limit.
        let repeated: Vec<i64> = (0..500).chain(0..500).collect();
        assert!(share(repeated, None, None).clean().is_ok());
        let long = "x".repeat(101);
        assert_eq!(
            share(vec![1], Some(&long), None).clean(),
            Err(ShareError::TitleTooLong)
        );
        assert_eq!(
            share(vec![1], Some("a\nb"), None).clean(),
            Err(ShareError::ControlChar)
        );
        assert_eq!(
            share(vec![1], None, Some(0)).clean(),
            Err(ShareError::BadDays)
        );
        assert_eq!(
            share(vec![1], None, Some(366)).clean(),
            Err(ShareError::BadDays)
        );
        assert!(share(vec![1], None, Some(365)).clean().is_ok());
    }

    #[test]
    fn tokens() {
        assert!(is_share_token("0123456789abcdef0123456789abcdef"));
        assert!(!is_share_token("0123456789ABCDEF0123456789abcdef"));
        assert!(!is_share_token("0123456789abcdef"));
        assert!(!is_share_token("../../etc/passwd/xxxxxxxxxxxxxxxx"));
        assert_eq!(share_path("ab"), "/s/ab");
    }
}
