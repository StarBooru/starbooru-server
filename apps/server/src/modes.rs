//! Private and public instances. Switching deletes what the new mode has no
//! use for ([`UNUSED`]) and converts votes to the score kind it counts, in
//! one transaction, after a backup unless the admin says not to.

use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use starbooru_core::mode::changes;
use starbooru_core::{Change, Mode, ModeSwitched, ScoreKind};
use tracing::{info, warn};

use crate::backup;
use crate::error::{AppError, AppResult};
use crate::scores;
use crate::settings;
use crate::state::AppState;

/// Serialises switches of mode and changes of score kind, and keeps votes
/// from landing halfway through one (votes take it shared).
const MODE_LOCK: i64 = 7277;

pub async fn lock(tx: &mut Transaction<'_, Postgres>) -> AppResult<()> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(MODE_LOCK)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Taken by a vote, so the score kind it reads holds until it is saved.
pub async fn lock_shared(tx: &mut Transaction<'_, Postgres>) -> AppResult<()> {
    sqlx::query("SELECT pg_advisory_xact_lock_shared($1)")
        .bind(MODE_LOCK)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Something a mode has no use for, which switching to it deletes.
struct Unused {
    /// The mode without a use for it.
    mode: Mode,
    /// Its name in a [`Change`].
    kind: &'static str,
    /// The table it is kept in, deleted from whole.
    table: &'static str,
}

/// What each mode has no use for. Public instances share posts by their own
/// address and don't reach other boorus, so their links and saved accounts
/// and searches go. Private instances have no use yet for anything public
/// ones keep.
const UNUSED: &[Unused] = &[
    Unused {
        mode: Mode::Public,
        kind: changes::SHARE_LINKS,
        table: "shares",
    },
    Unused {
        mode: Mode::Public,
        kind: changes::BOORU_ACCOUNTS,
        table: "booru_accounts",
    },
    Unused {
        mode: Mode::Public,
        kind: changes::REMOTE_SEARCHES,
        table: "remote_searches",
    },
];

fn unused_in(mode: Mode) -> impl Iterator<Item = &'static Unused> {
    UNUSED.iter().filter(move |u| u.mode == mode)
}

/// What switching to `mode` would delete, as [`Change`]s to be done.
async fn unused_counts(conn: &mut PgConnection, mode: Mode) -> AppResult<Vec<Change>> {
    let mut counts = Vec::new();
    for unused in unused_in(mode) {
        let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {}",
            unused.table
        )))
        .fetch_one(&mut *conn)
        .await?;
        counts.push((unused.kind, count));
    }
    Ok(changes_of(counts, false))
}

/// What switching to `to` would delete and convert, without changing a
/// thing. Empty when the server is in that mode already.
pub async fn preview(state: &AppState, to: Mode) -> AppResult<Vec<Change>> {
    let (mode, kind) = settings::scoring(&state.db).await?;
    if mode == to {
        return Ok(Vec::new());
    }
    let mut out = unused_counts(&mut *state.db.acquire().await?, to).await?;
    let after = kind_after(state, to).await?;
    if after != kind {
        out.extend(scores::plan(&state.db, after).await?);
    }
    Ok(out)
}

/// The score kind in force once in `to`.
async fn kind_after(state: &AppState, to: Mode) -> AppResult<ScoreKind> {
    match to {
        Mode::Public => Ok(ScoreKind::Likes),
        // Public servers store likes, so this is likes.
        Mode::Private => settings::chosen_score_kind(state).await,
    }
}

/// What [`settle`] found and did on this start.
#[derive(Debug, PartialEq, Eq)]
pub struct Settled {
    pub mode: Mode,
    pub kind: ScoreKind,
    /// Stored on this start, the first one with modes.
    pub first: bool,
    /// For the log: variables that weren't applied, or no longer are.
    pub warnings: Vec<String>,
}

/// Settles the mode and the score kind once, at start. The first time they
/// are stored as an admin change would store them: what `STARBOORU_MODE` and
/// `STARBOORU_SCORES` say (or the defaults) when that deletes and converts
/// nothing, else private with likes, which is what the data was before
/// modes. Nothing is converted here. From then on the variables have no
/// effect, and one that differs from what is stored gets a warning.
pub async fn settle(
    db: &PgPool,
    env_mode: Option<Mode>,
    env_kind: Option<ScoreKind>,
) -> AppResult<Settled> {
    let mut tx = db.begin().await?;
    lock(&mut tx).await?;
    let (stored_mode, stored_kind) = settings::stored_scoring(&mut *tx).await?;
    let mut warnings = Vec::new();
    let mut warned = (false, false);

    let mode = match stored_mode {
        Some(mode) => mode,
        None => {
            let wanted = env_mode.unwrap_or_default();
            let deletes = match wanted {
                Mode::Private => Vec::new(),
                Mode::Public => unused_counts(&mut tx, wanted).await?,
            };
            if deletes.is_empty() {
                wanted
            } else {
                warnings.push(not_applied(
                    "STARBOORU_MODE",
                    wanted.as_str(),
                    &deletes,
                    "this server stays private; switch it in Administration, \
                     which previews the switch and backs up first",
                ));
                warned.0 = true;
                Mode::Private
            }
        }
    };
    let kind = match stored_kind {
        Some(kind) => kind.in_mode(mode),
        None => {
            let wanted = env_kind.unwrap_or_default().in_mode(mode);
            let converts = match wanted {
                ScoreKind::Likes => Vec::new(),
                ScoreKind::Stars => scores::plan(&mut *tx, wanted).await?,
            };
            if converts.is_empty() {
                wanted
            } else {
                warnings.push(not_applied(
                    "STARBOORU_SCORES",
                    wanted.as_str(),
                    &converts,
                    "this server keeps likes; change it in Administration, \
                     which previews the conversion first",
                ));
                warned.1 = true;
                ScoreKind::Likes
            }
        }
    };

    let first = stored_mode.is_none() || stored_kind.is_none();
    if first {
        settings::save_mode(&mut *tx, mode).await?;
        settings::save_score_kind(&mut *tx, kind).await?;
    }
    tx.commit().await?;
    let env_mode = env_mode.filter(|_| !warned.0);
    let env_kind = env_kind.filter(|_| !warned.1);
    warnings.extend(ignored(env_mode, env_kind, mode, kind));
    Ok(Settled {
        mode,
        kind,
        first,
        warnings,
    })
}

/// The warning for a variable not applied on the first start, because of
/// what applying it would have done.
fn not_applied(variable: &str, value: &str, changes: &[Change], instead: &str) -> String {
    let would: Vec<&str> = changes.iter().map(|c| c.message.as_str()).collect();
    format!(
        "{variable}={value} was not applied, as then {}; {instead}",
        would.join(", and ")
    )
}

/// One warning for the variables set to something other than what the
/// server keeps, or `None` when they agree.
fn ignored(
    env_mode: Option<Mode>,
    env_kind: Option<ScoreKind>,
    mode: Mode,
    kind: ScoreKind,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(wanted) = env_mode.filter(|m| *m != mode) {
        parts.push(format!(
            "STARBOORU_MODE={} has no effect: this server keeps its stored mode, {}. \
             Switch it in Administration, which previews the switch and backs up first",
            wanted.as_str(),
            mode.as_str()
        ));
    }
    if let Some(wanted) = env_kind.filter(|k| *k != kind) {
        let why = if wanted.in_mode(mode) == wanted {
            format!(
                "this server keeps its stored score kind, {}. Change it in Administration, \
                 which previews the conversion first",
                kind.as_str()
            )
        } else {
            "a public server counts likes".to_owned()
        };
        parts.push(format!(
            "STARBOORU_SCORES={} has no effect: {why}",
            wanted.as_str()
        ));
    }
    (!parts.is_empty()).then(|| parts.join(". "))
}

/// `POST /admin/mode`: backs up (if `backup`), then deletes and converts
/// what `to` doesn't use and sets the mode, in one transaction.
pub async fn switch(state: &AppState, to: Mode, backup: bool) -> AppResult<ModeSwitched> {
    let Ok(_one) = state.mode_switch.try_lock() else {
        return Err(AppError::Conflict {
            code: "mode_switch_running",
            message: "the mode is being switched already".into(),
        });
    };
    if settings::mode(state).await? == to {
        return Err(AppError::Conflict {
            code: "same_mode",
            message: format!("this server is {} already", to.as_str()),
        });
    }
    if backup && !backup::available(state).await {
        return Err(AppError::Conflict {
            code: "backup_unavailable",
            message: "this server can't make backups (pg_dump is missing or turned off); \
                      switch without one, or set backups up first"
                .into(),
        });
    }
    let made = if backup {
        Some(backup_first(state).await?)
    } else {
        None
    };

    let mut tx = state.db.begin().await?;
    lock(&mut tx).await?;
    let (from, _) = settings::scoring(&mut *tx).await?;
    if from == to {
        return Err(AppError::Conflict {
            code: "same_mode",
            message: format!("this server is {} already", to.as_str()),
        });
    }
    let mut counts = Vec::new();
    for unused in unused_in(to) {
        let gone = sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM {}", unused.table)))
            .execute(&mut *tx)
            .await?
            .rows_affected();
        counts.push((unused.kind, i64::try_from(gone).unwrap_or(i64::MAX)));
    }
    let mut done = changes_of(counts, true);
    if to == Mode::Public {
        // Public instances count likes; stored so, the private one that may
        // come later starts from likes too.
        settings::save_score_kind(&mut *tx, ScoreKind::Likes).await?;
    }
    settings::save_mode(&mut *tx, to).await?;
    let (_, kind) = settings::scoring(&mut *tx).await?;
    done.extend(scores::convert(&mut tx, kind).await?);
    tx.commit().await?;

    info!("the server is {} now", to.as_str());
    for change in &done {
        info!("switching to {}: {}", to.as_str(), change.message);
    }
    Ok(ModeSwitched {
        settings: settings::current(state).await?,
        changes: done,
        backup: made,
    })
}

async fn backup_first(state: &AppState) -> AppResult<String> {
    match backup::create(state, false).await {
        Ok(made) => Ok(made.name),
        Err(AppError::Internal(detail)) => {
            warn!("the backup before switching modes failed: {detail}");
            Err(AppError::Conflict {
                code: "backup_failed",
                message: "the backup failed, so nothing changed; the server's log says why".into(),
            })
        }
        Err(other) => Err(other),
    }
}

/// `counts` as [`Change`]s, leaving out the ones with nothing to do.
/// `done` words them as done rather than to be done.
pub fn changes_of(counts: Vec<(&'static str, i64)>, done: bool) -> Vec<Change> {
    counts
        .into_iter()
        .filter_map(|(kind, count)| {
            let count = u64::try_from(count).ok().filter(|n| *n > 0)?;
            Some(Change {
                kind: kind.to_owned(),
                count,
                message: sentence(kind, count, done),
            })
        })
        .collect()
}

/// An English sentence for `count` of `kind`, e.g. "12 share links will be
/// deleted".
fn sentence(kind: &str, count: u64, done: bool) -> String {
    let one = count == 1;
    let deleted = match (done, one) {
        (false, _) => "will be deleted",
        (true, true) => "was deleted",
        (true, false) => "were deleted",
    };
    let become_ = if done { "became" } else { "will become" };
    let (thing, what) = match kind {
        changes::SHARE_LINKS => (
            if one { "share link" } else { "share links" },
            format!("{deleted}; on a public server a post is shared by its own address"),
        ),
        changes::BOORU_ACCOUNTS => (
            if one {
                "saved account on another booru"
            } else {
                "saved accounts on other boorus"
            },
            deleted.to_owned(),
        ),
        changes::REMOTE_SEARCHES => (
            if one {
                "saved search on other boorus"
            } else {
                "saved searches on other boorus"
            },
            deleted.to_owned(),
        ),
        changes::STAR_VOTES_CONVERTED => (
            if one { "star vote" } else { "star votes" },
            format!("{become_} likes and dislikes: 4 or 5 stars a like, 1 or 2 a dislike"),
        ),
        changes::STAR_VOTES_DROPPED => (
            if one {
                "vote of 3 stars"
            } else {
                "votes of 3 stars"
            },
            format!("{deleted}, as 3 stars is neither a like nor a dislike"),
        ),
        changes::LIKE_VOTES_CONVERTED => (
            if one {
                "like or dislike"
            } else {
                "likes and dislikes"
            },
            format!("{become_} stars: a like 5, a dislike 1"),
        ),
        other => (
            other,
            if done { "changed" } else { "will change" }.to_owned(),
        ),
    };
    format!("{count} {thing} {what}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentences_count_and_tense() {
        assert_eq!(
            sentence(changes::SHARE_LINKS, 12, false),
            "12 share links will be deleted; on a public server a post is shared by its own address"
        );
        assert_eq!(
            sentence(changes::BOORU_ACCOUNTS, 1, true),
            "1 saved account on another booru was deleted"
        );
        assert_eq!(
            sentence(changes::STAR_VOTES_DROPPED, 2, true),
            "2 votes of 3 stars were deleted, as 3 stars is neither a like nor a dislike"
        );
        assert_eq!(
            sentence(changes::LIKE_VOTES_CONVERTED, 3, false),
            "3 likes and dislikes will become stars: a like 5, a dislike 1"
        );
    }

    #[test]
    fn nothing_to_do_is_left_out() {
        let out = changes_of(
            vec![(changes::SHARE_LINKS, 0), (changes::REMOTE_SEARCHES, 2)],
            false,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, "remote_searches");
        assert_eq!(out[0].count, 2);
        assert!(changes_of(vec![(changes::SHARE_LINKS, -1)], true).is_empty());
    }

    #[test]
    fn warns_once_about_variables_that_differ() {
        use Mode::{Private, Public};
        use ScoreKind::{Likes, Stars};
        assert_eq!(ignored(None, None, Public, Likes), None);
        assert_eq!(ignored(Some(Public), Some(Likes), Public, Likes), None);
        let one = ignored(Some(Public), None, Private, Stars).unwrap();
        assert!(
            one.starts_with("STARBOORU_MODE=public has no effect"),
            "{one}"
        );
        assert!(one.contains("stored mode, private") && one.contains("Administration"));
        let both = ignored(Some(Private), Some(Stars), Public, Likes).unwrap();
        assert!(both.contains("STARBOORU_MODE=private") && both.contains("STARBOORU_SCORES=stars"));
        assert!(both.contains("a public server counts likes"), "{both}");
        let kind = ignored(None, Some(Likes), Private, Stars).unwrap();
        assert!(kind.contains("stored score kind, stars"), "{kind}");
    }

    #[test]
    fn not_applied_says_what_it_would_have_done() {
        let changes = changes_of(vec![(changes::SHARE_LINKS, 2)], false);
        let w = not_applied(
            "STARBOORU_MODE",
            "public",
            &changes,
            "this server stays private",
        );
        assert_eq!(
            w,
            "STARBOORU_MODE=public was not applied, as then 2 share links will be deleted; \
             on a public server a post is shared by its own address; this server stays private"
        );
    }

    #[test]
    fn every_unused_table_has_a_sentence() {
        for unused in UNUSED {
            assert!(!sentence(unused.kind, 2, false).ends_with("will change"));
            assert!(
                unused
                    .table
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'_')
            );
        }
    }
}
