//! Votes as likes or as stars. Each vote row says which it is (`stars` set
//! or not) and keeps, in `value`, the like it counts as. A post keeps both
//! tallies: `score`, the net of likes, and `star_sum` and `star_count` for
//! the stars. So a change of kind converts the votes in place, and nothing
//! else has to know which kind was in force when a vote was cast.

use sqlx::{PgExecutor, Postgres, Transaction};
use starbooru_core::mode::changes;
use starbooru_core::{Change, MAX_STARS, ScoreKind, VoteRequest, like_of, stars_of};

use crate::error::AppResult;
use crate::modes;

/// One account's vote on a post, as stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vote {
    /// The like it counts as: -1, 0 (3 stars) or 1.
    pub like: i16,
    /// 1 to 5 for a star vote.
    pub stars: Option<i16>,
}

impl Vote {
    /// A like or a dislike; `None` for 0.
    pub fn like(value: i16) -> Option<Self> {
        (value != 0).then_some(Self {
            like: value.signum(),
            stars: None,
        })
    }

    /// 1 to 5 stars; `None` for 0.
    pub fn stars(stars: u8) -> Option<Self> {
        (stars != 0).then_some(Self {
            like: like_of(stars),
            stars: Some(i16::from(stars.min(MAX_STARS))),
        })
    }

    /// A like (1) or dislike (-1) carried over from another site, as the
    /// kind in force has it.
    pub fn imported(kind: ScoreKind, like: i16) -> Option<Self> {
        match kind {
            ScoreKind::Likes => Self::like(like),
            ScoreKind::Stars if like == 0 => None,
            ScoreKind::Stars => Self::stars(stars_of(like)),
        }
    }
}

/// What `PUT /posts/{id}/vote` asks for under `kind`: `Ok(None)` removes
/// the vote. Under stars an app that sends only `value` is read as likes:
/// a like gives 5 stars and a dislike 1.
pub fn requested(kind: ScoreKind, req: VoteRequest) -> Result<Option<Vote>, &'static str> {
    match (kind, req.stars) {
        (ScoreKind::Likes, Some(_)) => {
            Err("this server counts likes and dislikes; send a value of -1, 0 or 1")
        }
        (ScoreKind::Stars, Some(stars)) if stars > MAX_STARS => {
            Err("a vote is 1 to 5 stars, or 0 to remove it")
        }
        (ScoreKind::Stars, Some(stars)) => Ok(Vote::stars(stars)),
        (_, None) if !(-1..=1).contains(&req.value) => Err("vote must be -1, 0 or 1"),
        (ScoreKind::Likes, None) => Ok(Vote::like(req.value)),
        (ScoreKind::Stars, None) => Ok(Vote::imported(ScoreKind::Stars, req.value)),
    }
}

/// How a post's tallies move when a vote goes from `old` to `new`: score,
/// star sum, star count.
pub fn tally_change(old: Option<Vote>, new: Option<Vote>) -> (i32, i32, i32) {
    let parts = |v: Option<Vote>| match v {
        None => (0, 0, 0),
        Some(v) => (
            i32::from(v.like),
            v.stars.map_or(0, i32::from),
            i32::from(v.stars.is_some()),
        ),
    };
    let (a, b) = (parts(old), parts(new));
    (b.0 - a.0, b.1 - a.1, b.2 - a.2)
}

/// What converting every vote to `to` would do.
pub async fn plan(db: impl PgExecutor<'_>, to: ScoreKind) -> AppResult<Vec<Change>> {
    let (likes, stars, threes): (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE stars IS NULL), \
                count(*) FILTER (WHERE stars IS NOT NULL AND stars <> 3), \
                count(*) FILTER (WHERE stars = 3) \
         FROM post_votes",
    )
    .fetch_one(db)
    .await?;
    let counts = match to {
        ScoreKind::Stars => vec![(changes::LIKE_VOTES_CONVERTED, likes)],
        ScoreKind::Likes => vec![
            (changes::STAR_VOTES_CONVERTED, stars),
            (changes::STAR_VOTES_DROPPED, threes),
        ],
    };
    Ok(modes::changes_of(counts, false))
}

/// Converts every vote that isn't of kind `to`, inside `tx`, and says what
/// it did. Likes become 5 stars and dislikes 1; stars of 4 or 5 become
/// likes, of 1 or 2 dislikes, and votes of 3 are deleted. `score` doesn't
/// move either way, as each vote already counted as its like.
pub async fn convert(tx: &mut Transaction<'_, Postgres>, to: ScoreKind) -> AppResult<Vec<Change>> {
    let counts = match to {
        ScoreKind::Stars => {
            let done = sqlx::query(
                "UPDATE post_votes SET stars = CASE WHEN value > 0 THEN $1 ELSE $2 END \
                 WHERE stars IS NULL",
            )
            .bind(i16::from(stars_of(1)))
            .bind(i16::from(stars_of(-1)))
            .execute(&mut **tx)
            .await?
            .rows_affected();
            if done > 0 {
                sqlx::query(
                    "UPDATE posts p SET star_sum = s.total, star_count = s.n \
                     FROM (SELECT post_id, sum(stars)::int AS total, count(*)::int AS n \
                           FROM post_votes GROUP BY post_id) s \
                     WHERE p.id = s.post_id AND (p.star_sum <> s.total OR p.star_count <> s.n)",
                )
                .execute(&mut **tx)
                .await?;
            }
            vec![(changes::LIKE_VOTES_CONVERTED, done)]
        }
        ScoreKind::Likes => {
            let dropped = sqlx::query("DELETE FROM post_votes WHERE stars = 3")
                .execute(&mut **tx)
                .await?
                .rows_affected();
            let done = sqlx::query("UPDATE post_votes SET stars = NULL WHERE stars IS NOT NULL")
                .execute(&mut **tx)
                .await?
                .rows_affected();
            sqlx::query("UPDATE posts SET star_sum = 0, star_count = 0 WHERE star_count <> 0")
                .execute(&mut **tx)
                .await?;
            vec![
                (changes::STAR_VOTES_CONVERTED, done),
                (changes::STAR_VOTES_DROPPED, dropped),
            ]
        }
    };
    let counts = counts
        .into_iter()
        .map(|(kind, n)| (kind, i64::try_from(n).unwrap_or(i64::MAX)))
        .collect();
    Ok(modes::changes_of(counts, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_a_vote_asks_for() {
        let like = |v| VoteRequest::like(v);
        let stars = |s| VoteRequest::stars(s);
        use ScoreKind::{Likes, Stars};
        assert_eq!(requested(Likes, like(1)), Ok(Vote::like(1)));
        assert_eq!(requested(Likes, like(0)), Ok(None));
        assert_eq!(
            requested(Likes, like(-1)).unwrap().unwrap(),
            Vote {
                like: -1,
                stars: None
            }
        );
        assert!(requested(Likes, like(2)).is_err());
        assert!(requested(Likes, stars(4)).is_err());
        assert_eq!(
            requested(Stars, stars(4)).unwrap().unwrap(),
            Vote {
                like: 1,
                stars: Some(4)
            }
        );
        assert_eq!(
            requested(Stars, stars(3)).unwrap().unwrap(),
            Vote {
                like: 0,
                stars: Some(3)
            }
        );
        assert_eq!(requested(Stars, stars(0)), Ok(None));
        assert!(requested(Stars, stars(6)).is_err());
        // An app that knows only likes, under stars.
        assert_eq!(requested(Stars, like(1)).unwrap().unwrap().stars, Some(5));
        assert_eq!(requested(Stars, like(-1)).unwrap().unwrap().stars, Some(1));
        assert_eq!(requested(Stars, like(0)), Ok(None));
        assert!(requested(Stars, like(-2)).is_err());
    }

    #[test]
    fn tallies_follow_the_vote() {
        let four = Vote::stars(4);
        let two = Vote::stars(2);
        assert_eq!(tally_change(None, four), (1, 4, 1));
        assert_eq!(tally_change(four, two), (-2, -2, 0));
        assert_eq!(tally_change(two, None), (1, -2, -1));
        assert_eq!(tally_change(Vote::stars(3), Vote::like(1)), (1, -3, -1));
        assert_eq!(tally_change(Vote::like(-1), Vote::like(1)), (2, 0, 0));
        assert_eq!(tally_change(None, None), (0, 0, 0));
    }
}
