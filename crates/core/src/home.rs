//! The home page: an optional front door with a title and a line under it,
//! over a moving background of posts, by default the most popular ones
//! rated safe. Admins turn it on and set it up; apps ask `GET /home`.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model::Rating;
use crate::profile::{is_invisible, one_line};
use crate::query::{Order, QueryError, SearchQuery, SortKey, names_order, parse_query};

/// How many posts the background may show.
pub const MIN_HOME_POSTS: u32 = 12;
pub const MAX_HOME_POSTS: u32 = 60;
pub const DEFAULT_HOME_POSTS: u32 = 36;
pub const MAX_HOME_TITLE_CHARS: usize = 64;
pub const MAX_HOME_SUBTITLE_CHARS: usize = 140;
pub const MAX_HOME_QUERY_CHARS: usize = 1000;

/// The home page as an admin sets it up (`ServerSettings::home`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HomeSettings {
    /// Off by default.
    #[serde(default)]
    pub enabled: bool,
    /// `None` shows the server's name.
    #[serde(default)]
    pub title: Option<String>,
    /// A short line under the title; `None` for none.
    #[serde(default)]
    pub subtitle: Option<String>,
    /// Which posts the background shows, in the search syntax; empty for
    /// the most popular. See [`background_query`].
    #[serde(default)]
    pub query: String,
    /// How many, [`MIN_HOME_POSTS`] to [`MAX_HOME_POSTS`].
    #[serde(default = "default_posts")]
    pub posts: u32,
}

fn default_posts() -> u32 {
    DEFAULT_HOME_POSTS
}

impl Default for HomeSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            title: None,
            subtitle: None,
            query: String::new(),
            posts: DEFAULT_HOME_POSTS,
        }
    }
}

/// `GET /home`: what the home page shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Home {
    pub enabled: bool,
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    /// The background, first the ones the search puts first. Empty when the
    /// page is off, and before signing in to a private server.
    #[serde(default)]
    pub posts: Vec<HomePost>,
}

/// One post of the home page's background: only what drawing it needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HomePost {
    pub id: i64,
    /// Signed, server-relative URL of the thumbnail (512 px at most).
    pub thumb_url: String,
    /// The same of a bigger copy, for big still images.
    #[serde(default)]
    pub sample_url: Option<String>,
    pub width: u32,
    pub height: u32,
    /// The picture's main colours as `#rrggbb`, most common first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub palette: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HomeError {
    #[error("the home page's title is longer than {MAX_HOME_TITLE_CHARS} characters")]
    TitleTooLong,
    #[error("the line under the title is longer than {MAX_HOME_SUBTITLE_CHARS} characters")]
    SubtitleTooLong,
    #[error("the home page's title or the line under it has a control or invisible character")]
    ControlChar,
    #[error("the background's search is longer than {MAX_HOME_QUERY_CHARS} characters")]
    QueryTooLong,
    #[error("the background shows live posts, not the recycle bin")]
    Deleted,
    #[error("the background shows {MIN_HOME_POSTS} to {MAX_HOME_POSTS} posts")]
    BadCount,
    #[error("the background's search: {0}")]
    Query(#[from] QueryError),
}

/// One line, trimmed; empty is none.
fn clean_line(raw: Option<&str>, max: usize, long: HomeError) -> Result<Option<String>, HomeError> {
    let line = one_line(raw.unwrap_or_default());
    if line.is_empty() {
        return Ok(None);
    }
    if line.chars().count() > max {
        return Err(long);
    }
    if line.chars().any(|c| c.is_control() || is_invisible(c)) {
        return Err(HomeError::ControlChar);
    }
    Ok(Some(line))
}

/// Trims and checks the home page's settings before they are saved.
pub fn clean_home(home: &HomeSettings) -> Result<HomeSettings, HomeError> {
    let title = clean_line(
        home.title.as_deref(),
        MAX_HOME_TITLE_CHARS,
        HomeError::TitleTooLong,
    )?;
    let subtitle = clean_line(
        home.subtitle.as_deref(),
        MAX_HOME_SUBTITLE_CHARS,
        HomeError::SubtitleTooLong,
    )?;
    let query = one_line(&home.query);
    if query.chars().count() > MAX_HOME_QUERY_CHARS {
        return Err(HomeError::QueryTooLong);
    }
    if parse_query(&query)?.deleted {
        return Err(HomeError::Deleted);
    }
    if !(MIN_HOME_POSTS..=MAX_HOME_POSTS).contains(&home.posts) {
        return Err(HomeError::BadCount);
    }
    Ok(HomeSettings {
        enabled: home.enabled,
        title,
        subtitle,
        query,
        posts: home.posts,
    })
}

/// The search behind the background: the admin's `query`, with
/// `rating:safe` added unless it names a rating (`rating:` or `-rating:`),
/// and sorted by score unless it names an order. Live posts only.
pub fn background_query(query: &str) -> Result<SearchQuery, QueryError> {
    let mut search = parse_query(query)?;
    if search.ratings.is_empty() && search.exclude_ratings.is_empty() {
        search.ratings.push(Rating::Safe);
    }
    if !names_order(query) {
        search.order = Order {
            key: SortKey::Score,
            descending: true,
        };
    }
    search.deleted = false;
    Ok(search)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(query: &str, posts: u32) -> HomeSettings {
        HomeSettings {
            enabled: true,
            title: Some("  My   booru ".into()),
            subtitle: Some(" ".into()),
            query: query.into(),
            posts,
        }
    }

    #[test]
    fn the_default_is_off_with_the_popular_safe_posts() {
        let settings: HomeSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings, HomeSettings::default());
        assert!(!settings.enabled);
        assert_eq!(settings.posts, DEFAULT_HOME_POSTS);
        let q = background_query(&settings.query).unwrap();
        assert_eq!(q.ratings, [Rating::Safe]);
        assert_eq!(q.order.key, SortKey::Score);
        assert!(q.order.descending);
    }

    #[test]
    fn safe_is_added_unless_the_query_names_a_rating() {
        let q = background_query("sky order:random").unwrap();
        assert_eq!(q.include, ["sky"]);
        assert_eq!(q.ratings, [Rating::Safe]);
        assert_eq!(q.order.key, SortKey::Random);
        let q = background_query("sky rating:q").unwrap();
        assert_eq!(q.ratings, [Rating::Questionable]);
        let q = background_query("-rating:e").unwrap();
        assert!(q.ratings.is_empty());
        assert_eq!(q.exclude_ratings, [Rating::Explicit]);
        assert_eq!(
            background_query("order:new").unwrap().order,
            Order::default()
        );
        assert!(background_query("rating:zzz").is_err());
    }

    #[test]
    fn cleans_the_settings() {
        let clean = clean_home(&home(" sky   order:score ", 24)).unwrap();
        assert_eq!(clean.title.as_deref(), Some("My booru"));
        assert_eq!(clean.subtitle, None);
        assert_eq!(clean.query, "sky order:score");
        assert_eq!(
            clean_home(&home("", MIN_HOME_POSTS - 1)),
            Err(HomeError::BadCount)
        );
        assert_eq!(
            clean_home(&home("", MAX_HOME_POSTS + 1)),
            Err(HomeError::BadCount)
        );
        assert!(clean_home(&home("", MAX_HOME_POSTS)).is_ok());
        assert_eq!(
            clean_home(&home("status:deleted", 24)),
            Err(HomeError::Deleted)
        );
        assert!(matches!(
            clean_home(&home("rating:zzz", 24)),
            Err(HomeError::Query(QueryError::BadRating(_)))
        ));
        let long = HomeSettings {
            title: Some("t".repeat(MAX_HOME_TITLE_CHARS + 1)),
            ..home("", 24)
        };
        assert_eq!(clean_home(&long), Err(HomeError::TitleTooLong));
        let long = HomeSettings {
            subtitle: Some("s".repeat(MAX_HOME_SUBTITLE_CHARS + 1)),
            ..home("", 24)
        };
        assert_eq!(clean_home(&long), Err(HomeError::SubtitleTooLong));
        let hidden = HomeSettings {
            title: Some("a\u{202E}b".into()),
            ..home("", 24)
        };
        assert_eq!(clean_home(&hidden), Err(HomeError::ControlChar));
        let query = "a".repeat(MAX_HOME_QUERY_CHARS + 1);
        assert_eq!(clean_home(&home(&query, 24)), Err(HomeError::QueryTooLong));
    }

    #[test]
    fn a_home_reads_and_writes_json() {
        let page = Home {
            enabled: true,
            title: "StarBooru".into(),
            subtitle: None,
            posts: vec![HomePost {
                id: 7,
                thumb_url: "/media/thumb/ab.jpg".into(),
                sample_url: None,
                width: 800,
                height: 600,
                palette: Vec::new(),
            }],
        };
        let json = serde_json::to_value(&page).unwrap();
        assert!(json["posts"][0].get("palette").is_none());
        assert_eq!(serde_json::from_value::<Home>(json).unwrap(), page);
        let bare: Home = serde_json::from_str(r#"{"enabled": false, "title": "S"}"#).unwrap();
        assert!(bare.posts.is_empty());
    }
}
