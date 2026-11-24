//! Private and public instances, and how posts are scored.
//!
//! An instance is private (the default) or public: open to other people with
//! their own accounts, and to people without one, who may browse. Switching
//! deletes what the new mode has no use for, after a preview of it.
//!
//! Votes are likes and dislikes, or 1 to 5 stars on a private instance.
//! Every star vote also counts as a like, a dislike or neither, so a post's
//! `score` means the same under both and old apps keep working.

use serde::{Deserialize, Serialize};

use crate::ServerSettings;

/// How an instance runs (`STARBOORU_MODE`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Everything needs an account.
    #[default]
    Private,
    /// Open to other people with their own accounts. People without one may
    /// browse, read-only.
    Public,
}

impl Mode {
    pub const ALL: [Mode; 2] = [Mode::Private, Mode::Public];

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Private => "private",
            Mode::Public => "public",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == s)
    }

    /// People without an account may browse.
    pub fn guests(self) -> bool {
        self == Mode::Public
    }
}

/// What a vote on a post is (`STARBOORU_SCORES`). A public instance always
/// counts likes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScoreKind {
    /// Like (1) or dislike (-1); a post's `score` is their sum.
    #[default]
    Likes,
    /// 1 to 5 stars; a post has their average and count.
    Stars,
}

impl ScoreKind {
    pub const ALL: [ScoreKind; 2] = [ScoreKind::Likes, ScoreKind::Stars];

    pub fn as_str(self) -> &'static str {
        match self {
            ScoreKind::Likes => "likes",
            ScoreKind::Stars => "stars",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// The kind in force in `mode`: what is chosen, except that a public
    /// instance counts likes.
    pub fn in_mode(self, mode: Mode) -> Self {
        match mode {
            Mode::Private => self,
            Mode::Public => ScoreKind::Likes,
        }
    }
}

/// Most stars one vote gives.
pub const MAX_STARS: u8 = 5;

/// The like a star vote counts as: 4 or 5 stars a like, 1 or 2 a dislike,
/// 3 neither (0). Also how stars become likes when the kind changes.
pub fn like_of(stars: u8) -> i16 {
    match stars {
        4.. => 1,
        1 | 2 => -1,
        _ => 0,
    }
}

/// The stars a like (1) or a dislike (-1) counts as when likes become stars.
pub fn stars_of(like: i16) -> u8 {
    if like > 0 { MAX_STARS } else { 1 }
}

/// `order:score` under stars ranks by a Bayesian average: the post's stars
/// as if it had [`PRIOR_VOTES`] votes of [`PRIOR_STARS`] already. A post
/// with few votes stays near the middle, so one vote of 5 doesn't beat fifty
/// of 4 (3.33 against 3.91), and a post nobody voted on sits at 3.
pub const PRIOR_STARS: i64 = 3;
pub const PRIOR_VOTES: i64 = 5;

/// The fair rating of a post whose `count` votes add up to `sum` stars.
pub fn fair_rating(sum: i64, count: i64) -> f64 {
    let sum = sum.saturating_add(PRIOR_STARS.saturating_mul(PRIOR_VOTES));
    let count = count.saturating_add(PRIOR_VOTES).max(1);
    sum as f64 / count as f64
}

/// The plain average of `count` votes adding up to `sum`, to two decimals;
/// `None` without votes.
pub fn average_stars(sum: i64, count: i64) -> Option<f64> {
    (count > 0).then(|| (sum as f64 / count as f64 * 100.0).round() / 100.0)
}

/// What a change of mode or of score kind deletes or converts, one kind of
/// thing at a time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// A machine name, one of [`changes`], for an app's own wording.
    pub kind: String,
    pub count: u64,
    /// The same in an English sentence.
    pub message: String,
}

/// The kinds of [`Change`].
pub mod changes {
    /// Share links, deleted going public.
    pub const SHARE_LINKS: &str = "share_links";
    /// Saved accounts on other boorus, deleted going public.
    pub const BOORU_ACCOUNTS: &str = "booru_accounts";
    /// Saved searches on other boorus, deleted going public.
    pub const REMOTE_SEARCHES: &str = "remote_searches";
    /// Star votes of 1, 2, 4 or 5 that become likes and dislikes.
    pub const STAR_VOTES_CONVERTED: &str = "star_votes_converted";
    /// Star votes of 3, which are neither and so are deleted.
    pub const STAR_VOTES_DROPPED: &str = "star_votes_dropped";
    /// Likes and dislikes that become stars.
    pub const LIKE_VOTES_CONVERTED: &str = "like_votes_converted";
}

/// `GET /admin/mode/preview?to=`: what switching to `to` would delete or
/// convert. Empty when nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModePreview {
    pub to: Mode,
    pub changes: Vec<Change>,
}

/// `POST /admin/mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeSwitch {
    pub to: Mode,
    /// Back the database up first (the default).
    #[serde(default = "yes")]
    pub backup: bool,
}

fn yes() -> bool {
    true
}

/// The answer to [`ModeSwitch`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeSwitched {
    /// The settings in force now.
    pub settings: ServerSettings,
    /// What was deleted and converted.
    pub changes: Vec<Change>,
    /// The backup made first, by name (see `GET /admin/backups`).
    pub backup: Option<String>,
}

/// `GET /admin/scores/preview?to=`: what changing the score kind to `to`
/// would convert or delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScorePreview {
    pub to: ScoreKind,
    pub changes: Vec<Change>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn names_round_trip() {
        for m in Mode::ALL {
            assert_eq!(Mode::parse(m.as_str()), Some(m));
            assert_eq!(serde_json::to_value(m).unwrap(), m.as_str());
        }
        for k in ScoreKind::ALL {
            assert_eq!(ScoreKind::parse(k.as_str()), Some(k));
            assert_eq!(serde_json::to_value(k).unwrap(), k.as_str());
        }
        assert_eq!(Mode::parse("Public"), None);
        assert_eq!(ScoreKind::parse(""), None);
        assert_eq!(Mode::default(), Mode::Private);
        assert_eq!(ScoreKind::default(), ScoreKind::Likes);
    }

    #[test]
    fn public_counts_likes() {
        assert_eq!(ScoreKind::Stars.in_mode(Mode::Private), ScoreKind::Stars);
        assert_eq!(ScoreKind::Stars.in_mode(Mode::Public), ScoreKind::Likes);
        assert_eq!(ScoreKind::Likes.in_mode(Mode::Public), ScoreKind::Likes);
        assert!(Mode::Public.guests() && !Mode::Private.guests());
    }

    #[test]
    fn stars_and_likes_map_both_ways() {
        let likes: Vec<i16> = (0..=5).map(like_of).collect();
        assert_eq!(likes, [0, -1, -1, 0, 1, 1]);
        assert_eq!((stars_of(1), stars_of(-1)), (5, 1));
        // A like converted to stars and back is the same like.
        for like in [-1, 1] {
            assert_eq!(like_of(stars_of(like)), like);
        }
    }

    #[test]
    fn a_fair_rating_needs_votes_to_move() {
        let one_five = fair_rating(5, 1);
        let fifty_fours = fair_rating(200, 50);
        assert!(fifty_fours > one_five, "{fifty_fours} {one_five}");
        assert!((one_five - 20.0 / 6.0).abs() < 1e-9);
        assert_eq!(fair_rating(0, 0), 3.0);
        assert!(fair_rating(1, 1) < fair_rating(0, 0));
        // One person's votes keep their order.
        let single: Vec<f64> = (1..=5).map(|s| fair_rating(s, 1)).collect();
        assert!(single.windows(2).all(|w| w[0] < w[1]));
        // Hostile numbers don't overflow or divide by zero.
        assert!(fair_rating(i64::MAX, i64::MAX).is_finite());
        assert!(fair_rating(0, -5).is_finite());
    }

    #[test]
    fn averages_to_two_decimals() {
        assert_eq!(average_stars(0, 0), None);
        assert_eq!(average_stars(13, 3), Some(4.33));
        assert_eq!(average_stars(5, 1), Some(5.0));
    }

    #[test]
    fn a_switch_backs_up_unless_told_not_to() {
        let s: ModeSwitch = serde_json::from_value(json!({ "to": "public" })).unwrap();
        assert_eq!(
            s,
            ModeSwitch {
                to: Mode::Public,
                backup: true
            }
        );
        let s: ModeSwitch =
            serde_json::from_value(json!({ "to": "private", "backup": false })).unwrap();
        assert!(!s.backup);
        assert!(serde_json::from_value::<ModeSwitch>(json!({ "to": "open" })).is_err());
    }
}
