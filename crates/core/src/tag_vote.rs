//! Votes on a post's tags, on a public instance: "this tag belongs here"
//! (1) or "it doesn't" (-1). A tag voted down to the threshold comes off
//! the post, and edits don't put it back until an admin undoes that.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const DEFAULT_TAG_VOTE_THRESHOLD: u32 = 3;
pub const MAX_TAG_VOTE_THRESHOLD: u32 = 100;

/// A tag's votes on one post.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagVote {
    /// Votes for minus votes against.
    pub score: i32,
    /// The caller's own: -1, 0 or 1.
    pub mine: i16,
}

/// `PUT /posts/{id}/tags/{name}/vote`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagVoteRequest {
    /// 1, -1, or 0 to take the vote back.
    pub value: i16,
}

/// Whether a tag at `score` comes off its post: at minus `threshold` or
/// below. A threshold of 0 never takes one off.
pub fn voted_off(score: i64, threshold: u32) -> bool {
    threshold > 0 && score <= -i64::from(threshold)
}

/// A tag the votes took off a post (`GET /admin/tag-removals`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagRemoval {
    pub id: i64,
    pub post_id: i64,
    pub tag: String,
    /// Its score when it came off.
    pub score: i32,
    pub removed_at: DateTime<Utc>,
    /// The post's thumbnail, signed; `None` without one.
    pub thumb_url: Option<String>,
}

/// `GET /admin/tag-removals`, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagRemovalPage {
    pub removals: Vec<TagRemoval>,
    pub page: u32,
    pub limit: u32,
    pub has_more: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tag_comes_off_at_the_threshold() {
        assert!(!voted_off(-2, 3));
        assert!(voted_off(-3, 3));
        assert!(voted_off(-40, 3));
        assert!(!voted_off(5, 3));
        assert!(voted_off(-1, 1));
        // 0 turns it off.
        assert!(!voted_off(i64::MIN, 0));
        assert!(voted_off(i64::MIN, MAX_TAG_VOTE_THRESHOLD));
    }

    #[test]
    fn votes_read_and_write() {
        let req: TagVoteRequest = serde_json::from_str(r#"{"value": -1}"#).unwrap();
        assert_eq!(req.value, -1);
        assert_eq!(
            serde_json::to_value(TagVote { score: -2, mine: 1 }).unwrap(),
            serde_json::json!({ "score": -2, "mine": 1 })
        );
    }
}
