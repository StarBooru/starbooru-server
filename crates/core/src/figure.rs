//! The people in a picture ("figures"). Each figure has a kind (girl, boy
//! or other), maybe a character name, the character they cosplay and the
//! franchise they come from, where their face is, and tags sorted into
//! slots: clothing by where it is worn, then body traits.
//!
//! A figure's tags are also the post's tags, so ordinary searches find
//! them. The count tags (`1girl`, `2boys`, `solo`...) follow the figures:
//! while a post has figures they are worked out from them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::tag::{TagError, normalize_tag};
use crate::vocab::split_category;

/// Most figures on one post.
pub const MAX_FIGURES: usize = 20;
/// Most tags on one figure, over all its slots.
pub const MAX_FIGURE_TAGS: usize = 64;
/// Added to a post while any figure cosplays someone.
pub const COSPLAY_TAG: &str = "cosplay";
const COSPLAY_SUFFIX: &str = "_(cosplay)";
/// A face box's edges are thousandths of the picture's width and height.
pub const FACE_SCALE: u16 = 1000;
/// The side of a kept face crop, in pixels: sharp at twice the 96 px circle
/// the apps show it in.
pub const FACE_SIZE: u32 = 192;

/// Where a face is in the picture: from its top-left corner, in thousandths
/// of the picture's width (`x`, `w`) and height (`y`, `h`), as the boxes of
/// panel translations are.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FaceBox {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl FaceBox {
    /// Inside the picture and not empty.
    pub fn is_valid(self) -> bool {
        let fits =
            |at: u16, len: u16| len > 0 && at.checked_add(len).is_some_and(|end| end <= FACE_SCALE);
        fits(self.x, self.w) && fits(self.y, self.h)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FigureKind {
    #[default]
    Girl,
    Boy,
    Other,
}

impl FigureKind {
    pub const ALL: [FigureKind; 3] = [FigureKind::Girl, FigureKind::Boy, FigureKind::Other];

    pub fn to_db(self) -> i16 {
        match self {
            FigureKind::Girl => 0,
            FigureKind::Boy => 1,
            FigureKind::Other => 2,
        }
    }

    pub fn from_db(v: i16) -> Option<Self> {
        match v {
            0 => Some(FigureKind::Girl),
            1 => Some(FigureKind::Boy),
            2 => Some(FigureKind::Other),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            FigureKind::Girl => "girl",
            FigureKind::Boy => "boy",
            FigureKind::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|k| s.eq_ignore_ascii_case(k.as_str()))
    }

    /// The count tag for `n` figures of this kind: `1girl`, `2boys`, `6+others`.
    fn count_tag(self, n: usize) -> String {
        let one = self.as_str();
        match n {
            1 => format!("1{one}"),
            2..=5 => format!("{n}{one}s"),
            _ => format!("6+{one}s"),
        }
    }
}

/// Where on a figure a tag goes: clothing first, then the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Slot {
    /// Hats, hair ornaments, headphones.
    Head,
    /// Tops, dresses, uniforms.
    Body,
    /// Skirts, trousers and legwear.
    Bottom,
    Shoes,
    /// Other clothing and accessories.
    Wear,
    /// Build.
    Shape,
    Eyes,
    Skin,
    Hair,
    Pose,
    /// Expression and anything else about the figure.
    Other,
}

impl Slot {
    pub const ALL: [Slot; 11] = [
        Slot::Head,
        Slot::Body,
        Slot::Bottom,
        Slot::Shoes,
        Slot::Wear,
        Slot::Shape,
        Slot::Eyes,
        Slot::Skin,
        Slot::Hair,
        Slot::Pose,
        Slot::Other,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Slot::Head => "head",
            Slot::Body => "body",
            Slot::Bottom => "bottom",
            Slot::Shoes => "shoes",
            Slot::Wear => "wear",
            Slot::Shape => "shape",
            Slot::Eyes => "eyes",
            Slot::Skin => "skin",
            Slot::Hair => "hair",
            Slot::Pose => "pose",
            Slot::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|k| s.eq_ignore_ascii_case(k.as_str()))
    }

    /// Clothing slots; the rest describe the body.
    pub fn is_clothing(self) -> bool {
        matches!(
            self,
            Slot::Head | Slot::Body | Slot::Bottom | Slot::Shoes | Slot::Wear
        )
    }

    /// Database value. 0 and 1 are the character and cosplay tags.
    pub fn to_db(self) -> i16 {
        match self {
            Slot::Head => 2,
            Slot::Body => 3,
            Slot::Bottom => 4,
            Slot::Shoes => 5,
            Slot::Wear => 6,
            Slot::Shape => 7,
            Slot::Eyes => 8,
            Slot::Skin => 9,
            Slot::Hair => 10,
            Slot::Pose => 11,
            Slot::Other => 12,
        }
    }

    pub fn from_db(v: i16) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.to_db() == v)
    }
}

/// One person (or creature) in a post.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Figure {
    pub kind: FigureKind,
    /// Who the figure is, as a character tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub character: Option<String>,
    /// The character the figure is dressed as, without `_(cosplay)`. The
    /// post gets the tag `name_(cosplay)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cosplay: Option<String>,
    /// The franchise (series) the figure comes from, a tag the post gets,
    /// made a copyright tag when it is new. For a cosplay, the franchise of
    /// the character they dress as. Always written, `null` for none, so a
    /// server can tell it from a field an older app leaves out ([`Sent`]).
    #[serde(default)]
    pub franchise: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<Slot, Vec<String>>,
    /// Where the figure's face is. Always written, like `franchise`.
    #[serde(default)]
    pub face: Option<FaceBox>,
    /// Signed, server-relative URL of the kept crop of the face, a square
    /// JPEG of [`FACE_SIZE`] pixels. Only in answers, and only once the
    /// server has made it; a request's is ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub face_url: Option<String>,
}

impl Figure {
    /// Every tag this figure puts on the post: the character, the cosplay
    /// tag, the franchise and the slot tags.
    pub fn tag_names(&self) -> Vec<String> {
        self.character
            .iter()
            .cloned()
            .chain(self.cosplay.as_deref().map(cosplay_tag))
            .chain(self.franchise.iter().cloned())
            .chain(self.tags.values().flatten().cloned())
            .collect()
    }

    pub(crate) fn tag_count(&self) -> usize {
        usize::from(self.character.is_some())
            + usize::from(self.cosplay.is_some())
            + usize::from(self.franchise.is_some())
            + self.tags.values().map(Vec::len).sum::<usize>()
    }

    /// The same person as `other`, as far as an app that doesn't know
    /// franchises and faces can tell: the same kind, character and cosplay.
    fn same_person(&self, other: &Figure) -> bool {
        self.kind == other.kind
            && self.character == other.character
            && self.cosplay == other.cosplay
    }
}

/// `PUT /posts/{id}/figures`: replaces the post's figures.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FiguresUpdate {
    pub figures: Vec<Figure>,
}

/// Which of the fields older apps don't know one figure of a `PUT
/// /posts/{id}/figures` body has. [`Figure`] always writes them, so an app
/// built on this crate sends both; one built before them sends neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sent {
    pub franchise: bool,
    pub face: bool,
}

impl Sent {
    pub const BOTH: Sent = Sent {
        franchise: true,
        face: true,
    };

    /// What each figure of a [`FiguresUpdate`] body sent, in order. A key
    /// with `null` counts as sent: it clears the field. The server still
    /// fills in the franchise of a person new to the post.
    pub fn read(body: &serde_json::Value) -> Vec<Sent> {
        body.get("figures")
            .and_then(|f| f.as_array())
            .map(|list| {
                list.iter()
                    .map(|figure| Sent {
                        franchise: figure.get("franchise").is_some(),
                        face: figure.get("face").is_some(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Gives each figure that left `franchise` or `face` out what the same
/// person had before, so saving from an app that doesn't know them changes
/// neither. The same person is an `old` figure of the same kind, character
/// and cosplay: the one in the same place if it is that, else the first
/// such one not taken yet. A figure with no such match keeps none.
pub fn keep_unsent(figures: &mut [Figure], sent: &[Sent], old: &[Figure]) {
    let mut taken = vec![false; old.len()];
    for (at, figure) in figures.iter_mut().enumerate() {
        let sent = sent.get(at).copied().unwrap_or(Sent::BOTH);
        if sent == Sent::BOTH {
            continue;
        }
        let free = |i: usize| taken.get(i).is_some_and(|t| !t);
        let here = old
            .get(at)
            .filter(|o| free(at) && o.same_person(figure))
            .map(|_| at);
        let found = here.or_else(|| {
            old.iter()
                .enumerate()
                .position(|(i, o)| free(i) && o.same_person(figure))
        });
        let Some((i, before)) = found.and_then(|i| Some((i, old.get(i)?))) else {
            continue;
        };
        if let Some(t) = taken.get_mut(i) {
            *t = true;
        }
        if !sent.franchise {
            figure.franchise.clone_from(&before.franchise);
        }
        if !sent.face {
            figure.face = before.face;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FigureError {
    #[error("too many people in one post (max {MAX_FIGURES})")]
    TooMany,
    #[error("too many tags on one person (max {MAX_FIGURE_TAGS})")]
    TooManyTags,
    #[error("a face box has to be inside the picture (edges from 0 to 1000) and not empty")]
    BadFace,
    #[error(transparent)]
    Tag(#[from] TagError),
}

/// The tag for cosplaying `name`: `name_(cosplay)`.
pub fn cosplay_tag(name: &str) -> String {
    format!("{}{COSPLAY_SUFFIX}", cosplay_name(name))
}

/// `name` without a `_(cosplay)` ending.
pub fn cosplay_name(tag: &str) -> &str {
    tag.strip_suffix(COSPLAY_SUFFIX)
        .filter(|n| !n.is_empty())
        .unwrap_or(tag)
}

/// The count tags for figures of these kinds, like `2girls`,
/// `multiple_girls`, `1boy`, or `solo` for a single figure.
pub fn count_tags(kinds: &[FigureKind]) -> Vec<String> {
    let mut out = Vec::new();
    for kind in FigureKind::ALL {
        let n = kinds.iter().filter(|k| **k == kind).count();
        if n > 0 {
            out.push(kind.count_tag(n));
        }
        if n > 1 {
            out.push(format!("multiple_{}s", kind.as_str()));
        }
    }
    if kinds.len() == 1 {
        out.push("solo".to_owned());
    }
    out
}

/// Whether `name` is one of the tags [`count_tags`] makes.
pub fn is_count_tag(name: &str) -> bool {
    if name == "solo" {
        return true;
    }
    FigureKind::ALL.into_iter().any(|kind| {
        let one = kind.as_str();
        name == format!("multiple_{one}s")
            || name == format!("1{one}")
            || name == format!("6+{one}s")
            || (2..=5).any(|n| name == format!("{n}{one}s"))
    })
}

/// The tags a post with these figures always has: the counts, and
/// `cosplay` when someone cosplays.
pub fn required_tags(figures: &[Figure]) -> Vec<String> {
    let kinds: Vec<FigureKind> = figures.iter().map(|f| f.kind).collect();
    let mut out = count_tags(&kinds);
    if figures.iter().any(|f| f.cosplay.is_some()) {
        out.push(COSPLAY_TAG.to_owned());
    }
    out
}

/// A name as a figure stores it: normalised, without a category prefix.
fn clean_name(raw: &str) -> Result<String, TagError> {
    let name = normalize_tag(raw)?;
    Ok(split_category(&name).1.to_owned())
}

/// Normalises figures for saving: names cleaned, empty values dropped, a
/// tag kept in its first place only, count tags left out (they are made
/// from the kinds), the URL of a face crop dropped (the server makes it)
/// and the limits checked.
pub fn clean_figures(figures: Vec<Figure>) -> Result<Vec<Figure>, FigureError> {
    if figures.len() > MAX_FIGURES {
        return Err(FigureError::TooMany);
    }
    let optional = |raw: Option<String>| -> Result<Option<String>, TagError> {
        match raw.as_deref().map(str::trim) {
            None | Some("") => Ok(None),
            Some(raw) => clean_name(raw).map(Some),
        }
    };
    let mut out = Vec::with_capacity(figures.len());
    for figure in figures {
        if figure.face.is_some_and(|b| !b.is_valid()) {
            return Err(FigureError::BadFace);
        }
        let character = optional(figure.character)?;
        let cosplay = optional(figure.cosplay)?.map(|c| cosplay_name(&c).to_owned());
        let mut seen: Vec<String> = character
            .iter()
            .cloned()
            .chain(cosplay.as_deref().map(cosplay_tag))
            .collect();
        let franchise = optional(figure.franchise)?.filter(|f| !seen.contains(f));
        seen.extend(franchise.iter().cloned());
        let mut tags = BTreeMap::new();
        for (slot, names) in figure.tags {
            let mut kept = Vec::new();
            for raw in &names {
                let name = clean_name(raw)?;
                if is_count_tag(&name) || seen.contains(&name) {
                    continue;
                }
                seen.push(name.clone());
                kept.push(name);
            }
            if !kept.is_empty() {
                tags.insert(slot, kept);
            }
        }
        let figure = Figure {
            kind: figure.kind,
            character,
            cosplay,
            franchise,
            tags,
            face: figure.face,
            face_url: None,
        };
        if figure.tag_count() > MAX_FIGURE_TAGS {
            return Err(FigureError::TooManyTags);
        }
        out.push(figure);
    }
    Ok(out)
}

/// [`clean_figures`] one figure at a time, for figures an AI found: one
/// that can't be saved is left out, not the others with it. At most
/// [`MAX_FIGURES`].
pub fn clean_each(figures: Vec<Figure>) -> Vec<Figure> {
    figures
        .into_iter()
        .take(MAX_FIGURES)
        .filter_map(|figure| clean_figures(vec![figure]).ok())
        .flatten()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(s: &str) -> Vec<FigureKind> {
        s.chars()
            .map(|c| match c {
                'g' => FigureKind::Girl,
                'b' => FigureKind::Boy,
                _ => FigureKind::Other,
            })
            .collect()
    }

    #[test]
    fn counts_follow_the_kinds() {
        assert_eq!(count_tags(&kinds("g")), ["1girl", "solo"]);
        assert_eq!(
            count_tags(&kinds("gbg")),
            ["2girls", "multiple_girls", "1boy"]
        );
        assert_eq!(count_tags(&kinds("ooo")), ["3others", "multiple_others"]);
        assert_eq!(count_tags(&kinds("bbbbbbb")), ["6+boys", "multiple_boys"]);
        assert!(count_tags(&[]).is_empty());
    }

    #[test]
    fn every_count_tag_is_known_and_in_the_vocabulary() {
        let vocab: Vec<&str> = crate::VOCAB.iter().map(|(t, _)| *t).collect();
        for kind in FigureKind::ALL {
            for n in 1..=8 {
                for tag in count_tags(&vec![kind; n]) {
                    assert!(is_count_tag(&tag), "{tag}");
                    assert!(vocab.contains(&tag.as_str()), "{tag} is not seeded");
                }
            }
        }
        for tag in ["girl", "2girl", "multiple_girl", "7girls", "solo_focus"] {
            assert!(!is_count_tag(tag), "{tag}");
        }
    }

    #[test]
    fn cosplay_tags() {
        assert_eq!(cosplay_tag("hatsune_miku"), "hatsune_miku_(cosplay)");
        assert_eq!(cosplay_tag("saber_(cosplay)"), "saber_(cosplay)");
        assert_eq!(cosplay_name("saber_(cosplay)"), "saber");
        assert_eq!(cosplay_name("_(cosplay)"), "_(cosplay)");
        let figures = vec![Figure {
            cosplay: Some("saber".into()),
            ..Figure::default()
        }];
        assert_eq!(required_tags(&figures), ["1girl", "solo", "cosplay"]);
        assert_eq!(figures[0].tag_names(), ["saber_(cosplay)"]);
    }

    #[test]
    fn cleans_figures() {
        let face = FaceBox {
            x: 100,
            y: 50,
            w: 200,
            h: 250,
        };
        let figure = Figure {
            kind: FigureKind::Boy,
            character: Some("char:Link".into()),
            cosplay: Some("  ".into()),
            franchise: Some("copyright:The Legend of Zelda".into()),
            tags: BTreeMap::from([
                (Slot::Hair, vec!["Blonde Hair".into(), "link".into()]),
                (Slot::Pose, vec!["standing".into(), "1boy".into()]),
                (
                    Slot::Other,
                    vec!["blonde_hair".into(), "the_legend_of_zelda".into()],
                ),
                (Slot::Shoes, vec![]),
            ]),
            face: Some(face),
            face_url: Some("/media/face/x.jpg".into()),
        };
        let clean = clean_figures(vec![figure]).unwrap();
        assert_eq!(
            clean,
            vec![Figure {
                kind: FigureKind::Boy,
                character: Some("link".into()),
                cosplay: None,
                franchise: Some("the_legend_of_zelda".into()),
                tags: BTreeMap::from([
                    (Slot::Hair, vec!["blonde_hair".into()]),
                    (Slot::Pose, vec!["standing".into()]),
                ]),
                face: Some(face),
                face_url: None,
            }]
        );
        assert_eq!(
            clean[0].tag_names(),
            ["link", "the_legend_of_zelda", "blonde_hair", "standing"]
        );
        // A franchise named like the character says nothing more.
        let same = Figure {
            character: Some("pikachu".into()),
            franchise: Some("Pikachu".into()),
            ..Figure::default()
        };
        assert_eq!(clean_figures(vec![same]).unwrap()[0].franchise, None);
    }

    #[test]
    fn limits() {
        assert_eq!(
            clean_figures(vec![Figure::default(); MAX_FIGURES + 1]),
            Err(FigureError::TooMany)
        );
        let many: Vec<String> = (0..=MAX_FIGURE_TAGS).map(|i| format!("t{i}")).collect();
        let figure = Figure {
            tags: BTreeMap::from([(Slot::Other, many)]),
            ..Figure::default()
        };
        assert_eq!(clean_figures(vec![figure]), Err(FigureError::TooManyTags));
        let bad = Figure {
            tags: BTreeMap::from([(Slot::Other, vec!["-x".into()])]),
            ..Figure::default()
        };
        assert!(matches!(clean_figures(vec![bad]), Err(FigureError::Tag(_))));
        let bad = Figure {
            franchise: Some("-x".into()),
            ..Figure::default()
        };
        assert!(matches!(clean_figures(vec![bad]), Err(FigureError::Tag(_))));
    }

    #[test]
    fn one_bad_figure_leaves_the_others() {
        let many: Vec<String> = (0..=MAX_FIGURE_TAGS).map(|i| format!("t{i}")).collect();
        let full = Figure {
            tags: BTreeMap::from([(Slot::Other, many)]),
            ..Figure::default()
        };
        let boy = Figure {
            kind: FigureKind::Boy,
            character: Some("Link".into()),
            ..Figure::default()
        };
        let kept = clean_each(vec![full, boy, Figure::default()]);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].character.as_deref(), Some("link"));
        assert_eq!(
            clean_each(vec![Figure::default(); MAX_FIGURES + 1]).len(),
            MAX_FIGURES
        );
    }

    #[test]
    fn face_boxes_stay_inside_the_picture() {
        let at = |x, y, w, h| FaceBox { x, y, w, h };
        assert!(at(0, 0, 1000, 1000).is_valid());
        assert!(at(999, 999, 1, 1).is_valid());
        for bad in [
            at(0, 0, 0, 10),
            at(0, 0, 10, 0),
            at(1, 0, 1000, 10),
            at(0, 995, 10, 6),
            at(u16::MAX, 0, 1, 1),
            at(0, 0, u16::MAX, u16::MAX),
        ] {
            assert!(!bad.is_valid(), "{bad:?}");
            let figure = Figure {
                face: Some(bad),
                ..Figure::default()
            };
            assert_eq!(clean_figures(vec![figure]), Err(FigureError::BadFace));
        }
        // Negative or fractional edges aren't a box at all.
        for json in [
            r#"{"kind":"girl","face":{"x":-1,"y":0,"w":5,"h":5}}"#,
            r#"{"kind":"girl","face":{"x":1.5,"y":0,"w":5,"h":5}}"#,
            r#"{"kind":"girl","face":{"x":1,"y":0,"w":5}}"#,
        ] {
            assert!(serde_json::from_str::<Figure>(json).is_err(), "{json}");
        }
    }

    #[test]
    fn slots_and_kinds_round_trip() {
        for slot in Slot::ALL {
            assert_eq!(Slot::from_db(slot.to_db()), Some(slot));
            assert_eq!(Slot::parse(slot.as_str()), Some(slot));
        }
        for kind in FigureKind::ALL {
            assert_eq!(FigureKind::from_db(kind.to_db()), Some(kind));
            assert_eq!(FigureKind::parse(kind.as_str()), Some(kind));
        }
        let json = serde_json::to_string(&Figure {
            tags: BTreeMap::from([(Slot::Shoes, vec!["boots".into()])]),
            ..Figure::default()
        })
        .unwrap();
        // The fields older apps don't know are always there, null or not.
        assert_eq!(
            json,
            r#"{"kind":"girl","franchise":null,"tags":{"shoes":["boots"]},"face":null}"#
        );
        // What an older server or app sends still reads.
        let old: Figure = serde_json::from_str(
            r#"{"kind":"boy","character":"link","tags":{"hair":["blonde_hair"]}}"#,
        )
        .unwrap();
        assert_eq!((old.franchise, old.face, old.face_url), (None, None, None));
    }

    fn named(kind: FigureKind, character: &str) -> Figure {
        Figure {
            kind,
            character: (!character.is_empty()).then(|| character.to_owned()),
            ..Figure::default()
        }
    }

    #[test]
    fn reads_which_new_fields_were_sent() {
        let body = serde_json::json!({"figures": [
            {"kind": "girl"},
            {"kind": "girl", "franchise": null, "face": null},
            {"kind": "boy", "face": {"x": 1, "y": 1, "w": 2, "h": 2}},
        ]});
        assert_eq!(
            Sent::read(&body),
            [
                Sent {
                    franchise: false,
                    face: false
                },
                Sent::BOTH,
                Sent {
                    franchise: false,
                    face: true
                },
            ]
        );
        assert!(Sent::read(&serde_json::json!({"figures": 3})).is_empty());
        assert!(Sent::read(&serde_json::json!([])).is_empty());
    }

    #[test]
    fn an_older_app_keeps_what_it_did_not_send() {
        let face = |x| {
            Some(FaceBox {
                x,
                y: 0,
                w: 10,
                h: 10,
            })
        };
        let with = |mut f: Figure, franchise: &str, x| {
            f.franchise = Some(franchise.to_owned());
            f.face = face(x);
            f
        };
        let old = vec![
            with(named(FigureKind::Girl, "saber"), "fate", 1),
            with(named(FigureKind::Girl, "rin"), "fate", 2),
            with(named(FigureKind::Girl, ""), "touhou", 3),
            with(named(FigureKind::Girl, ""), "touhou", 4),
        ];
        let neither = Sent {
            franchise: false,
            face: false,
        };

        // Saber left out: Rin is found further down, and the unnamed girls in order.
        let mut figures = vec![
            named(FigureKind::Girl, "rin"),
            named(FigureKind::Girl, ""),
            named(FigureKind::Girl, ""),
        ];
        keep_unsent(&mut figures, &[neither; 3], &old);
        assert_eq!(figures[0], old[1]);
        assert_eq!(figures[1], old[2]);
        assert_eq!(figures[2], old[3]);

        // Another kind, character or cosplay is someone else: nothing kept.
        let mut figures = vec![
            named(FigureKind::Boy, "saber"),
            named(FigureKind::Girl, "artoria"),
        ];
        keep_unsent(&mut figures, &[neither; 2], &old);
        assert_eq!((figures[0].face, figures[1].face), (None, None));
        assert_eq!(figures[1].franchise, None);

        // What was sent stays as sent, a null included.
        let mut figures = vec![named(FigureKind::Girl, "saber")];
        figures[0].face = face(9);
        let sent = Sent {
            franchise: false,
            face: true,
        };
        keep_unsent(&mut figures, &[sent], &old);
        assert_eq!(figures[0].franchise.as_deref(), Some("fate"));
        assert_eq!(figures[0].face, face(9));
        let mut figures = vec![named(FigureKind::Girl, "saber")];
        keep_unsent(&mut figures, &[Sent::BOTH], &old);
        assert_eq!(
            (figures[0].franchise.as_deref(), figures[0].face),
            (None, None)
        );
        // A figure past the list of what was sent counts as sending both.
        let mut figures = vec![named(FigureKind::Girl, "saber")];
        keep_unsent(&mut figures, &[], &old);
        assert_eq!(figures[0].face, None);
    }
}
