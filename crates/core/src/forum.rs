//! The forum of a public instance: categories set by admins, threads in
//! them and posts in threads. Posts are plain text; Markdown in them is
//! kept as typed, for the apps to show.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::profile::{UserCard, has_control, is_invisible, lines, one_line};

pub const MAX_TITLE_CHARS: usize = 120;
pub const MAX_BODY_CHARS: usize = 10_000;
pub const MAX_CATEGORY_NAME_CHARS: usize = 64;
pub const MAX_CATEGORY_DESCRIPTION_CHARS: usize = 500;
pub const MAX_CATEGORIES: usize = 100;
/// An account may post once in this many seconds, and [`POSTS_PER_DAY`]
/// times in a day. Admins aren't held to either.
pub const POST_INTERVAL_SECS: i64 = 15;
pub const POSTS_PER_DAY: i64 = 100;
pub const DEFAULT_THREADS_LIMIT: u32 = 30;
pub const DEFAULT_POSTS_LIMIT: u32 = 50;
pub const MAX_FORUM_LIMIT: u32 = 100;
/// The categories a forum starts with, the first time the server goes
/// public: name and description.
pub const DEFAULT_CATEGORIES: [(&str, &str); 3] = [
    ("General", "Anything about the pictures here"),
    (
        "Requests",
        "Looking for a picture, or for someone to tag one",
    ),
    ("Site", "Questions and ideas about this server"),
];

/// `GET /forum`: a category with what is in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForumCategory {
    pub id: i64,
    pub name: String,
    pub description: String,
    /// Categories are listed by this, lowest first, then by name.
    pub position: i32,
    pub threads: u64,
    /// The newest post in any of its threads; `None` while it has none.
    pub last_activity_at: Option<DateTime<Utc>>,
}

/// `POST /forum/categories` (admins).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewCategory {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// After the others when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<i32>,
}

/// `PATCH /forum/categories/{id}` (admins): fields left out stay.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategoryUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<i32>,
}

/// A thread, as lists show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForumThread {
    pub id: i64,
    pub category_id: i64,
    pub title: String,
    /// `None` once the account is deleted.
    pub author: Option<UserCard>,
    pub created_at: DateTime<Utc>,
    /// When its newest post was written.
    pub last_activity_at: DateTime<Utc>,
    /// Its posts, the first and deleted ones included.
    pub posts: u64,
    /// Listed before the others in its category.
    pub pinned: bool,
    /// Only admins may post in it.
    pub locked: bool,
}

/// `GET /forum/threads`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadPage {
    pub threads: Vec<ForumThread>,
    pub page: u32,
    pub limit: u32,
    pub has_more: bool,
}

/// One post in a thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForumPost {
    pub id: i64,
    pub thread_id: i64,
    /// `None` once the account is deleted.
    pub author: Option<UserCard>,
    /// Plain text, Markdown as typed. Empty when deleted.
    pub body: String,
    pub created_at: DateTime<Utc>,
    /// The last time its author changed it.
    pub edited_at: Option<DateTime<Utc>>,
    /// Deleted: it keeps its place, so the thread still reads in order.
    pub deleted: bool,
}

/// `GET /forum/threads/{id}`: the thread and a page of its posts, oldest
/// first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadView {
    pub thread: ForumThread,
    pub posts: Vec<ForumPost>,
    pub page: u32,
    pub limit: u32,
    pub has_more: bool,
}

/// `POST /forum/threads`: a thread with its first post.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewThread {
    pub category_id: i64,
    pub title: String,
    pub body: String,
}

/// `POST /forum/threads/{id}/posts` and `PATCH /forum/posts/{id}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForumPostBody {
    pub body: String,
}

/// `PATCH /forum/threads/{id}`: its title (its author or an admin), and
/// whether it is pinned or locked and where it sits (admins). Fields left
/// out stay.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked: Option<bool>,
    /// Moves the thread to another category.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category_id: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ForumError {
    #[error("give the thread a title")]
    EmptyTitle,
    #[error("the title is longer than {MAX_TITLE_CHARS} characters")]
    TitleTooLong,
    #[error("the title contains a control or invisible character")]
    TitleControlChar,
    #[error("write something first")]
    EmptyBody,
    #[error("the post is longer than {MAX_BODY_CHARS} characters")]
    BodyTooLong,
    #[error("the post contains a control character")]
    BodyControlChar,
    #[error("give the category a name")]
    EmptyName,
    #[error("the name is longer than {MAX_CATEGORY_NAME_CHARS} characters")]
    NameTooLong,
    #[error("the name contains a control or invisible character")]
    NameControlChar,
    #[error("the description is longer than {MAX_CATEGORY_DESCRIPTION_CHARS} characters")]
    DescriptionTooLong,
    #[error("the description contains a control character")]
    DescriptionControlChar,
}

/// One line of 1 to `max` characters with no control or invisible ones.
fn line(raw: &str, max: usize, errors: [ForumError; 3]) -> Result<String, ForumError> {
    let [empty, long, control] = errors;
    let text = one_line(raw);
    if text.is_empty() {
        return Err(empty);
    }
    if text.chars().count() > max {
        return Err(long);
    }
    if text.chars().any(|c| c.is_control() || is_invisible(c)) {
        return Err(control);
    }
    Ok(text)
}

/// A thread's title: one line, 1 to [`MAX_TITLE_CHARS`] characters.
pub fn clean_title(raw: &str) -> Result<String, ForumError> {
    line(
        raw,
        MAX_TITLE_CHARS,
        [
            ForumError::EmptyTitle,
            ForumError::TitleTooLong,
            ForumError::TitleControlChar,
        ],
    )
}

/// A category's name: one line, 1 to [`MAX_CATEGORY_NAME_CHARS`].
pub fn clean_category_name(raw: &str) -> Result<String, ForumError> {
    line(
        raw,
        MAX_CATEGORY_NAME_CHARS,
        [
            ForumError::EmptyName,
            ForumError::NameTooLong,
            ForumError::NameControlChar,
        ],
    )
}

/// A post: its lines kept, trimmed, 1 to [`MAX_BODY_CHARS`] characters.
pub fn clean_body(raw: &str) -> Result<String, ForumError> {
    let text = lines(raw);
    if text.is_empty() {
        return Err(ForumError::EmptyBody);
    }
    if text.chars().count() > MAX_BODY_CHARS {
        return Err(ForumError::BodyTooLong);
    }
    if has_control(&text) {
        return Err(ForumError::BodyControlChar);
    }
    Ok(text)
}

/// A category's description: like a post, but it may be empty.
pub fn clean_description(raw: &str) -> Result<String, ForumError> {
    let text = lines(raw);
    if text.chars().count() > MAX_CATEGORY_DESCRIPTION_CHARS {
        return Err(ForumError::DescriptionTooLong);
    }
    if has_control(&text) {
        return Err(ForumError::DescriptionControlChar);
    }
    Ok(text)
}

/// A post of the booru named in a forum post: `>>123` or `post #123`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PostMention {
    /// Where the mention starts and ends in the text, in bytes.
    pub start: usize,
    pub end: usize,
    pub id: i64,
}

/// The posts a forum post names, for the apps to link. The server keeps the
/// text as written.
pub fn post_mentions(text: &str) -> Vec<PostMention> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(rest) = bytes.get(at..).filter(|r| !r.is_empty()) {
        let after_word = at > 0
            && bytes
                .get(at.saturating_sub(1))
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
        let prefix = if rest.starts_with(b">>") {
            Some(2)
        } else if !after_word
            && rest
                .get(..6)
                .is_some_and(|p| p.eq_ignore_ascii_case(b"post #"))
        {
            Some(6)
        } else {
            None
        };
        if let Some(mention) = prefix.and_then(|skip| mention_at(text, at, skip)) {
            at = mention.end;
            out.push(mention);
        } else {
            at = at.saturating_add(1);
        }
    }
    out
}

/// The mention whose `skip` bytes of prefix start at `at`, if digits for a
/// post id follow and end a word.
fn mention_at(text: &str, at: usize, skip: usize) -> Option<PostMention> {
    let bytes = text.as_bytes();
    let from = at.checked_add(skip)?;
    let digits = bytes
        .get(from..)?
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    let end = from.checked_add(digits)?;
    let joined = bytes
        .get(end)
        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
    if digits == 0 || digits > 18 || joined {
        return None;
    }
    let id: i64 = text.get(from..end)?.parse().ok().filter(|id| *id > 0)?;
    Some(PostMention { start: at, end, id })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_and_names_are_one_line() {
        assert_eq!(
            clean_title("  Best   tags\nfor cats "),
            Ok("Best tags for cats".into())
        );
        assert_eq!(clean_title(" "), Err(ForumError::EmptyTitle));
        assert!(clean_title(&"t".repeat(MAX_TITLE_CHARS)).is_ok());
        assert_eq!(
            clean_title(&"t".repeat(MAX_TITLE_CHARS + 1)),
            Err(ForumError::TitleTooLong)
        );
        assert_eq!(clean_title("a\u{0}b"), Err(ForumError::TitleControlChar));
        assert_eq!(clean_title("a\u{202E}b"), Err(ForumError::TitleControlChar));
        assert_eq!(clean_category_name(" Site "), Ok("Site".into()));
        assert_eq!(clean_category_name(""), Err(ForumError::EmptyName));
        assert_eq!(
            clean_category_name(&"n".repeat(MAX_CATEGORY_NAME_CHARS + 1)),
            Err(ForumError::NameTooLong)
        );
    }

    #[test]
    fn bodies_keep_their_lines_and_markdown() {
        assert_eq!(
            clean_body("\r\n# Hi\r\n\r\n* one\n* `two`\n  "),
            Ok("# Hi\n\n* one\n* `two`".into())
        );
        assert_eq!(clean_body(" \n\t "), Err(ForumError::EmptyBody));
        assert!(clean_body(&"b".repeat(MAX_BODY_CHARS)).is_ok());
        assert_eq!(
            clean_body(&"b".repeat(MAX_BODY_CHARS + 1)),
            Err(ForumError::BodyTooLong)
        );
        assert_eq!(clean_body("bell\u{7}"), Err(ForumError::BodyControlChar));
        assert_eq!(clean_description("  "), Ok(String::new()));
        assert_eq!(
            clean_description(&"d".repeat(MAX_CATEGORY_DESCRIPTION_CHARS + 1)),
            Err(ForumError::DescriptionTooLong)
        );
    }

    #[test]
    fn finds_the_posts_a_post_names() {
        let ids = |text: &str| post_mentions(text).iter().map(|m| m.id).collect::<Vec<_>>();
        assert_eq!(ids(">>12 and post #7, Post #8."), [12, 7, 8]);
        let m = post_mentions("see >>123!");
        assert_eq!(
            m,
            [PostMention {
                start: 4,
                end: 9,
                id: 123
            }]
        );
        assert_eq!("see >>123!".get(m[0].start..m[0].end), Some(">>123"));
        // Not ids: no digits, glued to a word, zero, too long.
        assert_eq!(ids(">> 5 >>x repost #4 >>12abc >>0"), Vec::<i64>::new());
        assert_eq!(ids(">>99999999999999999999"), Vec::<i64>::new());
        assert_eq!(ids(">>>5"), [5]);
        // Byte offsets stay on character boundaries.
        let text = "ciò >>5 è";
        let m = post_mentions(text);
        assert_eq!(text.get(m[0].start..m[0].end), Some(">>5"));
        assert!(post_mentions("").is_empty());
        assert!(post_mentions("post #").is_empty());
    }
}
