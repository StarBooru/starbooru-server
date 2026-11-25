//! The people in the collection: its characters (tags in the `character`
//! category on live posts), each with the franchise it belongs to and a
//! face to show, for Explore and a page of everyone.

use serde::{Deserialize, Serialize};

/// People in one page of `GET /people` when no `limit` is given.
pub const DEFAULT_PEOPLE_LIMIT: u32 = 60;
/// The most people one page of `GET /people` holds.
pub const MAX_PEOPLE_LIMIT: u32 = 100;
/// Franchises in one page of `GET /franchises` when no `limit` is given.
pub const DEFAULT_FRANCHISE_LIMIT: u32 = 50;
/// The most franchises one page of `GET /franchises` holds.
pub const MAX_FRANCHISE_LIMIT: u32 = 200;

/// A character of the collection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Person {
    /// The character tag; a search for it finds their posts.
    pub name: String,
    /// Live posts with the character. In Explore with a search, those of
    /// the posts it looked at; in `popular_characters`, the voted posts.
    pub posts: i64,
    /// The franchise the character belongs to, when the collection says.
    #[serde(default)]
    pub franchise: Option<String>,
    /// Signed URL of a kept face crop of the character, a square JPEG.
    #[serde(default)]
    pub face_url: Option<String>,
    /// Signed URL of the thumbnail of one of their posts, when there is no
    /// face to show.
    #[serde(default)]
    pub thumb_url: Option<String>,
    /// `popular_characters` only: the net upvotes their posts got in the
    /// window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub votes: Option<i64>,
}

/// How `GET /people` sorts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PeopleOrder {
    /// Most posts first.
    #[default]
    Posts,
    Name,
    /// By franchise, the franchise whose people have the most posts first,
    /// then most posts first within each; people without one come last.
    Franchise,
}

impl PeopleOrder {
    pub fn as_str(self) -> &'static str {
        match self {
            PeopleOrder::Posts => "posts",
            PeopleOrder::Name => "name",
            PeopleOrder::Franchise => "franchise",
        }
    }
}

/// `GET /people`'s query, as the client sends it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeopleQuery {
    /// Characters whose name holds this, written as a tag (`hatsune miku`
    /// and `hatsune_miku` alike). Empty for everyone.
    #[serde(default)]
    pub q: String,
    /// Only the people of this franchise. `Some("")` for the people without
    /// one, `None` for everyone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub franchise: Option<String>,
    #[serde(default)]
    pub order: PeopleOrder,
    pub page: u32,
    pub limit: u32,
}

impl Default for PeopleQuery {
    fn default() -> Self {
        Self {
            q: String::new(),
            franchise: None,
            order: PeopleOrder::default(),
            page: 1,
            limit: DEFAULT_PEOPLE_LIMIT,
        }
    }
}

/// `GET /people`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeoplePage {
    pub people: Vec<Person>,
    pub page: u32,
    pub limit: u32,
    pub has_more: bool,
}

/// A franchise with people in the collection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Franchise {
    /// The franchise's tag.
    pub name: String,
    /// Its characters in use.
    pub people: i64,
    /// Live posts with the franchise's tag.
    pub posts: i64,
}

/// `GET /franchises`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FranchisePage {
    pub franchises: Vec<Franchise>,
    pub page: u32,
    pub limit: u32,
    pub has_more: bool,
    /// People without a franchise (`GET /people?franchise=`).
    pub without: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_person_reads_and_writes() {
        let person = Person {
            name: "hatsune_miku".into(),
            posts: 3,
            franchise: Some("vocaloid".into()),
            face_url: Some("/media/face/ab.jpg".into()),
            ..Person::default()
        };
        let json = serde_json::to_value(&person).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"name": "hatsune_miku", "posts": 3, "franchise": "vocaloid",
                               "face_url": "/media/face/ab.jpg", "thumb_url": null})
        );
        assert_eq!(serde_json::from_value::<Person>(json).unwrap(), person);
        let order: PeopleOrder = serde_json::from_str(r#""franchise""#).unwrap();
        assert_eq!(order.as_str(), "franchise");
    }
}
