//! A tag blacklist: tags, and `rating:` terms, whose posts an account
//! doesn't want to see. The server leaves those posts out of searches,
//! Explore and related posts, unless the search names them itself.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model::Rating;
use crate::query::{QueryError, SearchQuery, parse_query};
use crate::tag::{TagError, normalize_tag};

/// Most entries an account's list holds.
pub const MAX_BLACKLIST: usize = 500;
/// Most entries in a guest's `exclude` and in the admin's default, which
/// guests get: a guest sends their list with every read, and an address
/// has to stay short.
pub const MAX_EXCLUDE: usize = 100;

/// `GET` and `PUT /auth/blacklist`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blacklist {
    /// Tag names, and ratings as `rating:explicit`. A `PUT` normalises the
    /// names and resolves aliases, as tagging does.
    #[serde(default)]
    pub entries: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BlacklistError {
    #[error("a blacklist holds at most {0} entries")]
    TooMany(usize),
    #[error(
        "`{0}` isn't a tag or a rating: a blacklist hides tags, and ratings as rating:explicit"
    )]
    NotATag(String),
    #[error(transparent)]
    Tag(#[from] TagError),
    #[error(transparent)]
    Query(#[from] QueryError),
}

/// One entry of a blacklist.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Entry {
    /// Hides posts with the tag, or a tag under it in the tree.
    Tag(String),
    Rating(Rating),
}

impl Entry {
    /// Reads one entry: a tag, folded as tagging folds it (`Blue Sky` is
    /// `blue_sky`) and without a category prefix (`artist:`), as in a
    /// search; or a `rating:` term. Other search terms are refused.
    pub fn parse(raw: &str) -> Result<Self, BlacklistError> {
        let term = normalize_tag(raw)?;
        let query = parse_query(&term)?;
        if let [tag] = query.include.as_slice() {
            let only = SearchQuery {
                include: query.include.clone(),
                ..SearchQuery::default()
            };
            if query == only {
                return Ok(Entry::Tag(tag.clone()));
            }
        }
        if let [rating] = query.ratings.as_slice() {
            let only = SearchQuery {
                ratings: query.ratings.clone(),
                ..SearchQuery::default()
            };
            if query == only {
                return Ok(Entry::Rating(*rating));
            }
        }
        Err(BlacklistError::NotATag(raw.trim().to_owned()))
    }

    /// The entry as a list keeps it: the tag, or `rating:<rating>`.
    pub fn as_entry(&self) -> String {
        match self {
            Entry::Tag(name) => name.clone(),
            Entry::Rating(rating) => format!("rating:{}", rating.as_str()),
        }
    }
}

/// Cleans a list for saving: each entry read by [`Entry::parse`], blank
/// ones skipped, repeats dropped, at most `max`.
pub fn clean_blacklist(entries: &[String], max: usize) -> Result<Vec<Entry>, BlacklistError> {
    // Repeats are dropped, but a list can't be any length on the way in.
    if entries.len() > max.saturating_mul(2) {
        return Err(BlacklistError::TooMany(max));
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for raw in entries.iter().filter(|e| !e.trim().is_empty()) {
        let entry = Entry::parse(raw)?;
        if seen.insert(entry.clone()) {
            if out.len() >= max {
                return Err(BlacklistError::TooMany(max));
            }
            out.push(entry);
        }
    }
    Ok(out)
}

/// Reads a guest's `exclude` parameter: entries separated by spaces, at
/// most [`MAX_EXCLUDE`].
pub fn parse_exclude(param: &str) -> Result<Vec<Entry>, BlacklistError> {
    let raw: Vec<String> = param
        .split_whitespace()
        .take(MAX_EXCLUDE.saturating_mul(2).saturating_add(1))
        .map(str::to_owned)
        .collect();
    clean_blacklist(&raw, MAX_EXCLUDE)
}

/// The tags and the ratings of a list as kept. An entry that doesn't read
/// is skipped.
pub fn split_entries(entries: &[String]) -> (Vec<String>, Vec<Rating>) {
    let mut tags = Vec::new();
    let mut ratings = Vec::new();
    for entry in entries {
        match Entry::parse(entry) {
            Ok(Entry::Tag(name)) => tags.push(name),
            Ok(Entry::Rating(rating)) => ratings.push(rating),
            Err(_) => {}
        }
    }
    (tags, ratings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn kept(list: &[&str]) -> Result<Vec<String>, BlacklistError> {
        clean_blacklist(&entries(list), MAX_BLACKLIST)
            .map(|v| v.iter().map(Entry::as_entry).collect())
    }

    #[test]
    fn reads_tags_and_ratings() {
        assert_eq!(
            Entry::parse(" Blue Sky "),
            Ok(Entry::Tag("blue_sky".into()))
        );
        assert_eq!(
            Entry::parse("artist:Some_One"),
            Ok(Entry::Tag("some_one".into()))
        );
        assert_eq!(Entry::parse("re:zero"), Ok(Entry::Tag("re:zero".into())));
        assert_eq!(
            Entry::parse("rating:E"),
            Ok(Entry::Rating(Rating::Explicit))
        );
        assert_eq!(Entry::parse("rating:g"), Ok(Entry::Rating(Rating::Safe)));
        assert_eq!(
            Entry::Rating(Rating::Questionable).as_entry(),
            "rating:questionable"
        );
    }

    #[test]
    fn refuses_what_isnt_a_tag_or_a_rating() {
        for term in [
            "type:video",
            "order:score",
            "order:new",
            "status:active",
            "exact:true",
            "color:red",
            "date:2024",
            "width:>10",
            "{girl}",
            "text:hello",
        ] {
            assert!(
                matches!(Entry::parse(term), Err(BlacklistError::NotATag(_))),
                "{term}: {:?}",
                Entry::parse(term)
            );
        }
        assert!(matches!(
            Entry::parse("-cat"),
            Err(BlacklistError::Tag(TagError::BadPrefix(_)))
        ));
        assert!(matches!(
            Entry::parse("rating:x"),
            Err(BlacklistError::Query(QueryError::BadRating(_)))
        ));
        assert!(Entry::parse("").is_err());
        let message = Entry::parse("type:video").unwrap_err().to_string();
        assert!(message.contains("`type:video`"), "{message}");
    }

    #[test]
    fn cleans_a_list() {
        assert_eq!(
            kept(&["Cat", "", "  ", "cat", "rating:e", "rating:explicit", "dog"]),
            Ok(entries(&["cat", "rating:explicit", "dog"]))
        );
        let many: Vec<String> = (0..=MAX_BLACKLIST).map(|i| format!("t{i}")).collect();
        assert_eq!(
            clean_blacklist(&many, MAX_BLACKLIST),
            Err(BlacklistError::TooMany(MAX_BLACKLIST))
        );
        // Repeats don't count towards the limit, up to a point.
        let repeated = vec!["cat".to_owned(); MAX_BLACKLIST * 2];
        assert_eq!(clean_blacklist(&repeated, MAX_BLACKLIST).unwrap().len(), 1);
        let flood = vec!["cat".to_owned(); MAX_BLACKLIST * 2 + 1];
        assert!(clean_blacklist(&flood, MAX_BLACKLIST).is_err());
    }

    #[test]
    fn reads_a_guests_exclude() {
        let list = parse_exclude("cat  Rating:Q dog cat").unwrap();
        assert_eq!(
            list,
            vec![
                Entry::Tag("cat".into()),
                Entry::Rating(Rating::Questionable),
                Entry::Tag("dog".into())
            ]
        );
        assert_eq!(parse_exclude("").unwrap(), Vec::new());
        let long = (0..=MAX_EXCLUDE)
            .map(|i| format!("t{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            parse_exclude(&long),
            Err(BlacklistError::TooMany(MAX_EXCLUDE))
        );
        assert!(parse_exclude("cat type:video").is_err());
    }

    #[test]
    fn splits_a_kept_list() {
        let (tags, ratings) = split_entries(&entries(&["cat", "rating:explicit", "type:x", "dog"]));
        assert_eq!(tags, ["cat", "dog"]);
        assert_eq!(ratings, [Rating::Explicit]);
    }

    #[test]
    fn reads_and_writes_json() {
        let list: Blacklist = serde_json::from_str("{}").unwrap();
        assert!(list.entries.is_empty());
        assert_eq!(
            serde_json::to_value(Blacklist {
                entries: entries(&["cat"])
            })
            .unwrap(),
            serde_json::json!({ "entries": ["cat"] })
        );
    }
}
