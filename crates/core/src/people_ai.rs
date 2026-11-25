//! The AI that finds the people in a picture: the box maths of its face
//! finders, how what a tagger read off each person becomes a figure, and
//! the prompt for an AI provider. Pure, so the worker and the server share
//! it and it is tested without models.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::ai_worker::{FoundPerson, FoundTag};
use crate::figure::{
    FACE_SCALE, FaceBox, Figure, FigureKind, MAX_FIGURES, Slot, clean_figures, cosplay_name,
};
use crate::model::TagCategory;
use crate::tag::normalize_tag;

/// A box in a picture's pixels, with the finder's score.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Detection {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub score: f32,
}

impl Detection {
    /// From a centre and a size, as detectors give them.
    pub fn centred(cx: f32, cy: f32, w: f32, h: f32, score: f32) -> Self {
        Self {
            x: cx - w / 2.0,
            y: cy - h / 2.0,
            w,
            h,
            score,
        }
    }

    fn is_usable(&self) -> bool {
        [self.x, self.y, self.w, self.h, self.score]
            .iter()
            .all(|v| v.is_finite())
            && self.w > 0.0
            && self.h > 0.0
    }

    fn area(&self) -> f32 {
        self.w.max(0.0) * self.h.max(0.0)
    }

    fn overlap(&self, other: &Self) -> f32 {
        let w = (self.x + self.w).min(other.x + other.w) - self.x.max(other.x);
        let h = (self.y + self.h).min(other.y + other.h) - self.y.max(other.y);
        w.max(0.0) * h.max(0.0)
    }

    /// Intersection over union.
    pub fn iou(&self, other: &Self) -> f32 {
        let union = self.area() + other.area() - self.overlap(other);
        if union > 0.0 {
            self.overlap(other) / union
        } else {
            0.0
        }
    }

    /// How much of the smaller box the two share: one finder's box for a
    /// face can sit inside the other's, larger one.
    pub fn iom(&self, other: &Self) -> f32 {
        let smaller = self.area().min(other.area());
        if smaller > 0.0 {
            self.overlap(other) / smaller
        } else {
            0.0
        }
    }
}

/// How a picture goes into a detector's square input: scaled to
/// `width` × `height` and put in the top-left corner, the rest filled.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Letterbox {
    pub scale: f32,
    pub width: u32,
    pub height: u32,
}

impl Letterbox {
    /// The picture's longer side fit to `fit` pixels.
    pub fn new(width: u32, height: u32, fit: u32) -> Self {
        let fit = fit.max(1);
        let scale = fit as f32 / width.max(height).max(1) as f32;
        let side = |v: u32| ((v as f32 * scale).round() as u32).clamp(1, fit);
        Self {
            scale,
            width: side(width),
            height: side(height),
        }
    }

    /// A box found on the detector's input, in the picture's pixels.
    pub fn to_picture(&self, d: Detection) -> Detection {
        let s = self.scale;
        Detection {
            x: d.x / s,
            y: d.y / s,
            w: d.w / s,
            h: d.h / s,
            score: d.score,
        }
    }
}

/// Most boxes looked at in one picture, best first: a detector can give
/// thousands of weak ones.
const MAX_CANDIDATES: usize = 300;

/// Non-maximum suppression: the best box first, then each that neither
/// overlaps a kept one by more than `iou` nor lies more than `iom` inside
/// one. Unusable boxes go.
pub fn suppress(mut found: Vec<Detection>, iou: f32, iom: f32) -> Vec<Detection> {
    found.retain(Detection::is_usable);
    found.sort_by(|a, b| b.score.total_cmp(&a.score));
    found.truncate(MAX_CANDIDATES);
    let mut kept: Vec<Detection> = Vec::new();
    for d in found {
        if kept.iter().all(|k| d.iou(k) <= iou && d.iom(k) <= iom) {
            kept.push(d);
        }
    }
    kept
}

/// A box as a figure's face: cut to the picture, in thousandths. `None`
/// when too little of it is inside.
pub fn face_box(d: Detection, width: u32, height: u32) -> Option<FaceBox> {
    if width == 0 || height == 0 || !d.is_usable() {
        return None;
    }
    let (w, h) = (width as f32, height as f32);
    let scale = f32::from(FACE_SCALE);
    let edge = |v: f32, len: f32| ((v.clamp(0.0, len) / len) * scale).round() as u16;
    let (x0, x1) = (edge(d.x, w), edge(d.x + d.w, w));
    let (y0, y1) = (edge(d.y, h), edge(d.y + d.h, h));
    let face = FaceBox {
        x: x0,
        y: y0,
        w: x1.saturating_sub(x0),
        h: y1.saturating_sub(y0),
    };
    face.is_valid().then_some(face)
}

/// A rectangle in a picture's pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// A face box in pixels: left, top, width, height.
fn pixels(face: FaceBox, width: u32, height: u32) -> (f32, f32, f32, f32) {
    let scale = f32::from(FACE_SCALE);
    let (w, h) = (width as f32, height as f32);
    (
        f32::from(face.x) / scale * w,
        f32::from(face.y) / scale * h,
        f32::from(face.w) / scale * w,
        f32::from(face.h) / scale * h,
    )
}

/// The part of `[x0, x1) × [y0, y1)` inside the picture, at least a pixel.
fn inside(x0: f32, y0: f32, x1: f32, y1: f32, width: u32, height: u32) -> Option<Rect> {
    if width == 0 || height == 0 || ![x0, y0, x1, y1].iter().all(|v| v.is_finite()) {
        return None;
    }
    // Float to integer casts saturate, so nothing here can overflow.
    let left = (x0.max(0.0) as u32).min(width - 1);
    let top = (y0.max(0.0) as u32).min(height - 1);
    let right = (x1.round().max(0.0) as u32).clamp(left + 1, width);
    let bottom = (y1.round().max(0.0) as u32).clamp(top + 1, height);
    Some(Rect {
        x: left,
        y: top,
        w: right - left,
        h: bottom - top,
    })
}

/// The face's crop for the tagger: a square half as large again as the
/// face's longer side, around its middle, cut to the picture.
pub fn face_crop(face: FaceBox, width: u32, height: u32) -> Option<Rect> {
    let (x, y, w, h) = pixels(face, width, height);
    let half = w.max(h) * 0.75;
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    inside(cx - half, cy - half, cx + half, cy + half, width, height)
}

/// The person below a face, for reading their clothes: four faces wide,
/// from just above the head to about the knees of a standing adult. In a
/// group it stops halfway to a neighbour's face and above the head of
/// someone in front (`others`, which may hold `face` itself), so their
/// clothes aren't read as this person's.
pub fn body_crop(face: FaceBox, others: &[FaceBox], width: u32, height: u32) -> Option<Rect> {
    let (x, y, w, h) = pixels(face, width, height);
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    let (mut left, mut top) = (cx - 2.0 * w, y - 0.4 * h);
    let (mut right, mut bottom) = (cx + 2.0 * w, y + 7.0 * h);
    for other in others.iter().filter(|o| **o != face) {
        let (ox, oy, ow, oh) = pixels(*other, width, height);
        let (ocx, ocy) = (ox + ow / 2.0, oy + oh / 2.0);
        if !(left..right).contains(&ocx) || !(top..bottom).contains(&ocy) {
            continue;
        }
        if (ocy - cy).abs() < h.max(oh) {
            // Beside: share the space between the two.
            if ocx > cx {
                right = right.min((cx + ocx) / 2.0);
            } else {
                left = left.max((cx + ocx) / 2.0);
            }
        } else if ocy > cy {
            bottom = bottom.min(oy);
        }
    }
    // Never narrower than the face itself.
    left = left.min(x);
    right = right.max(x + w);
    top = top.min(y);
    bottom = bottom.max(y + h);
    inside(left, top, right, bottom, width, height)
}

/// What the tagger needs to see a person in a face's crop: `1girl`, `1boy`
/// and the like, or else a strong `solo`. Below, it was a glove or a sign.
const PERSON_MIN: f32 = 0.5;
const SOLO_MIN: f32 = 0.8;
/// Over this, the face is an animal's or a thing's.
const NO_HUMANS_MAX: f32 = 0.5;
/// What a tag read off a crop needs to go on the figure. Higher than for a
/// whole picture: an empty slot is better than a wrong one.
pub const TAG_MIN: f32 = 0.5;
/// What a character read off the face's crop needs when a second reading
/// agrees: the body's crop, or the post's own tags.
const CHARACTER_MIN: f32 = 0.85;
/// What it needs alone. A moustached hunter in a green hat read as Luigi at
/// 0.91 in our test set; the characters both crops named read at 0.93 or
/// more.
const CHARACTER_SURE: f32 = 0.95;
/// Most tags put on one figure: the strongest readings.
const MAX_PERSON_TAGS: usize = 24;
/// What `1girl` or `1boy` needs for the figure to be a girl or a boy.
const KIND_MIN: f32 = 0.5;
/// Over this, the character is someone dressed as them.
const COSPLAY_MIN: f32 = 0.35;
const PHOTO_MIN: f32 = 0.5;

/// Tags a crop of one person gets when it holds a person.
const PERSON_TAGS: &[&str] = &[
    "1girl",
    "1boy",
    "1other",
    "male_focus",
    "multiple_girls",
    "multiple_boys",
];
/// Tags that say how a crop is framed rather than who is in it.
const FRAMING: &[&str] = &[
    "portrait",
    "upper_body",
    "close-up",
    "cowboy_shot",
    "full_body",
    "lower_body",
    "from_side",
    "from_behind",
    "from_above",
    "from_below",
    "head_out_of_frame",
    "out_of_frame",
    "cropped_torso",
    "cropped_legs",
];
/// Read off the face's crop; the rest off the body's.
const FACE_SLOTS: &[Slot] = &[Slot::Hair, Slot::Eyes, Slot::Skin, Slot::Other];

/// Whether the tagger sees a person in a face's crop. Measured on
/// public-domain photos and drawings, where it turns away what the face
/// finders mistook for a face (a blurred sign, a cartoon wolf) and keeps
/// every real one.
pub fn is_person(tags: &[FoundTag]) -> bool {
    let person = PERSON_TAGS
        .iter()
        .map(|name| score(tags, name))
        .fold(0.0, f32::max);
    (person >= PERSON_MIN || score(tags, "solo") >= SOLO_MIN)
        && score(tags, "no_humans") < NO_HUMANS_MAX
}

fn score(tags: &[FoundTag], name: &str) -> f32 {
    tags.iter()
        .filter(|t| t.name == name)
        .map(|t| t.score)
        .fold(0.0, f32::max)
}

/// Tags that say a picture is a photograph, or drawn like one.
const PHOTO_TAGS: [&str; 2] = ["realistic", "photorealistic"];

/// Whether the people are in a photograph: the post's tags say so, or the
/// tagger does of at least half of them. A small or masked face can read as
/// a drawing; a semi-realistic drawing can have a face that reads as a photo.
fn is_photo(people: &[FoundPerson], on_post: &impl Fn(&str) -> bool) -> bool {
    let reads = |p: &FoundPerson| {
        PHOTO_TAGS
            .iter()
            .any(|t| score(&p.face_tags, t).max(score(&p.body_tags, t)) >= PHOTO_MIN)
    };
    let photos = people.iter().filter(|p| reads(p)).count();
    PHOTO_TAGS.iter().any(|t| on_post(t)) || (photos > 0 && photos * 2 >= people.len())
}

/// What was read off the people in a picture, as figures with their faces.
/// `on_post` says whether the post already has a tag (from the tagging
/// tool, which saw the whole picture); `place` puts a tag in its slot,
/// `None` for a tag that doesn't describe a person. Franchises are left to
/// the caller, which knows the collection.
pub fn figures_from(
    people: &[FoundPerson],
    on_post: impl Fn(&str) -> bool,
    place: impl Fn(&str) -> Option<Slot>,
) -> Vec<Figure> {
    let photo = is_photo(people, &on_post);
    people
        .iter()
        .map(|p| figure_from(p, photo, &on_post, &place))
        .collect()
}

fn figure_from(
    person: &FoundPerson,
    photo: bool,
    on_post: &impl Fn(&str) -> bool,
    place: &impl Fn(&str) -> Option<Slot>,
) -> Figure {
    let both = |name: &str| score(&person.face_tags, name).max(score(&person.body_tags, name));
    // Who they are comes from the face: in a group the body's crop can hold
    // a neighbour.
    let face = |name: &str| score(&person.face_tags, name);
    let (girl, boy) = (face("1girl"), face("1boy").max(face("male_focus")));
    let kind = if girl >= KIND_MIN && girl > boy {
        FigureKind::Girl
    } else if boy >= KIND_MIN && boy > girl && !photo {
        FigureKind::Boy
    } else {
        // On photographs the tagger, trained on drawings, called most women
        // in our group photos boys, some at 0.97. Its girls were right.
        FigureKind::Other
    };
    let named_by_body = |name: &str| {
        person
            .body_tags
            .iter()
            .any(|t| t.name == name && t.category == Some(TagCategory::Character))
    };
    let character = person
        .face_tags
        .iter()
        .filter(|t| t.category == Some(TagCategory::Character))
        .filter(|t| {
            t.score >= CHARACTER_SURE
                || t.score >= CHARACTER_MIN && (named_by_body(&t.name) || on_post(&t.name))
        })
        .max_by(|a, b| a.score.total_cmp(&b.score))
        .and_then(|t| normalize_tag(&t.name).ok());
    let dressed_up = photo || both("cosplay") >= COSPLAY_MIN;
    let (character, cosplay) = match character {
        Some(c) if dressed_up => (None, Some(cosplay_name(&c).to_owned())),
        other => (other, None),
    };

    let mut found: Vec<(f32, Slot, String)> = Vec::new();
    let names = person.face_tags.iter().chain(&person.body_tags);
    for tag in names.filter(|t| t.category.is_none_or(|c| c == TagCategory::General)) {
        let raw = tag.name.as_str();
        // Kaomoji such as `^_^` aren't tags here.
        let Ok(name) = normalize_tag(raw) else {
            continue;
        };
        if FRAMING.contains(&name.as_str()) || found.iter().any(|(_, _, n)| *n == name) {
            continue;
        }
        let Some(slot) = place(&name) else {
            continue;
        };
        // Neighbours share the body's crop in a group, so looks count on
        // the face's; clothes only show on the body's.
        let read = if FACE_SLOTS.contains(&slot) {
            score(&person.face_tags, raw)
        } else if matches!(slot, Slot::Head | Slot::Wear) {
            both(raw)
        } else {
            score(&person.body_tags, raw)
        };
        if read >= TAG_MIN {
            found.push((read, slot, name));
        }
    }
    found.sort_by(|a, b| b.0.total_cmp(&a.0));
    found.truncate(MAX_PERSON_TAGS);
    let mut tags: BTreeMap<Slot, Vec<String>> = BTreeMap::new();
    for (_, slot, name) in found {
        tags.entry(slot).or_default().push(name);
    }
    Figure {
        kind,
        character,
        cosplay,
        tags,
        face: Some(person.face),
        ..Figure::default()
    }
}

/// The slot the tag tree gives a tag, from the tags above it, nearest
/// first: `Some(None)` when it puts it under something that doesn't
/// describe a person (the picture's setting or style), `None` when it says
/// nothing (no parent, or only the admin's own tags).
fn slot_in_tree<S: AsRef<str>>(ancestors: &[S]) -> Option<Option<Slot>> {
    ancestors.iter().find_map(|name| {
        Some(Some(match name.as_ref() {
            "headwear" => Slot::Head,
            "bodywear" => Slot::Body,
            "bottomwear" | "legwear" => Slot::Bottom,
            "footwear" => Slot::Shoes,
            "accessories" | "clothing" => Slot::Wear,
            "body_shape" => Slot::Shape,
            "body_features" | "body" | "expression" => Slot::Other,
            "hair" => Slot::Hair,
            "eyes" => Slot::Eyes,
            "skin" => Slot::Skin,
            "pose" => Slot::Pose,
            "people" | "setting" | "style" => return Some(None),
            _ => return None,
        }))
    })
}

/// Last words of clothing and looks the starter tree doesn't have, such as
/// `white_shirt` or `pleated_skirt`.
const LAST_WORDS: &[(&str, Slot)] = &[
    ("hat", Slot::Head),
    ("cap", Slot::Head),
    ("beret", Slot::Head),
    ("helmet", Slot::Head),
    ("hood", Slot::Head),
    ("crown", Slot::Head),
    ("tiara", Slot::Head),
    ("hairband", Slot::Head),
    ("headband", Slot::Head),
    ("hairclip", Slot::Head),
    ("headphones", Slot::Head),
    ("headdress", Slot::Head),
    ("headwear", Slot::Head),
    ("headpiece", Slot::Head),
    ("veil", Slot::Head),
    ("shirt", Slot::Body),
    ("blouse", Slot::Body),
    ("sweater", Slot::Body),
    ("hoodie", Slot::Body),
    ("jacket", Slot::Body),
    ("coat", Slot::Body),
    ("vest", Slot::Body),
    ("top", Slot::Body),
    ("dress", Slot::Body),
    ("uniform", Slot::Body),
    ("serafuku", Slot::Body),
    ("kimono", Slot::Body),
    ("armor", Slot::Body),
    ("bodysuit", Slot::Body),
    ("bikini", Slot::Body),
    ("swimsuit", Slot::Body),
    ("leotard", Slot::Body),
    ("apron", Slot::Body),
    ("cardigan", Slot::Body),
    ("blazer", Slot::Body),
    ("collar", Slot::Body),
    ("sleeves", Slot::Body),
    ("robe", Slot::Body),
    ("skirt", Slot::Bottom),
    ("shorts", Slot::Bottom),
    ("pants", Slot::Bottom),
    ("jeans", Slot::Bottom),
    ("trousers", Slot::Bottom),
    ("thighhighs", Slot::Bottom),
    ("pantyhose", Slot::Bottom),
    ("socks", Slot::Bottom),
    ("kneehighs", Slot::Bottom),
    ("leggings", Slot::Bottom),
    ("legwear", Slot::Bottom),
    ("shoes", Slot::Shoes),
    ("boots", Slot::Shoes),
    ("sneakers", Slot::Shoes),
    ("heels", Slot::Shoes),
    ("sandals", Slot::Shoes),
    ("loafers", Slot::Shoes),
    ("slippers", Slot::Shoes),
    ("footwear", Slot::Shoes),
    ("barefoot", Slot::Shoes),
    ("gloves", Slot::Wear),
    ("scarf", Slot::Wear),
    ("necktie", Slot::Wear),
    ("bowtie", Slot::Wear),
    ("choker", Slot::Wear),
    ("necklace", Slot::Wear),
    ("earrings", Slot::Wear),
    ("jewelry", Slot::Wear),
    ("belt", Slot::Wear),
    ("bag", Slot::Wear),
    ("wristwatch", Slot::Wear),
    ("mask", Slot::Wear),
    ("glasses", Slot::Wear),
    ("eyewear", Slot::Wear),
    ("bracelet", Slot::Wear),
    ("cape", Slot::Wear),
    ("cloak", Slot::Wear),
    ("ribbon", Slot::Wear),
    ("bow", Slot::Wear),
    ("eyes", Slot::Eyes),
    ("pupils", Slot::Eyes),
    ("eyelashes", Slot::Eyes),
    ("eyebrows", Slot::Eyes),
    ("skin", Slot::Skin),
    ("freckles", Slot::Skin),
    ("tanlines", Slot::Skin),
    ("breasts", Slot::Shape),
    ("abs", Slot::Shape),
    ("smile", Slot::Other),
    ("grin", Slot::Other),
    ("blush", Slot::Other),
    ("frown", Slot::Other),
    ("smirk", Slot::Other),
    ("tears", Slot::Other),
    ("mouth", Slot::Other),
    ("lips", Slot::Other),
    ("ears", Slot::Other),
    ("beard", Slot::Hair),
    ("mustache", Slot::Hair),
    ("bangs", Slot::Hair),
    ("sidelocks", Slot::Hair),
    ("ponytail", Slot::Hair),
    ("twintails", Slot::Hair),
    ("braid", Slot::Hair),
    ("ahoge", Slot::Hair),
];
/// Hair things that are worn rather than grown.
const HAIR_WEAR: &[&str] = &[
    "ornament",
    "ribbon",
    "bow",
    "clip",
    "band",
    "flower",
    "tie",
    "pin",
    "bobbles",
    "scrunchie",
    "stick",
    "tubes",
    "bell",
];

/// The slot of a tag the tag tree hasn't placed, from its words. `None`
/// when they don't tell.
pub fn slot_by_name(name: &str) -> Option<Slot> {
    let words: Vec<&str> = name.split('_').filter(|w| !w.is_empty()).collect();
    let last = *words.last()?;
    if words.contains(&"hair") && HAIR_WEAR.contains(&last) {
        return Some(Slot::Head);
    }
    if words.first() == Some(&"hair") || last == "hair" {
        return Some(Slot::Hair);
    }
    LAST_WORDS
        .iter()
        .find(|(word, _)| *word == last)
        .map(|(_, slot)| *slot)
}

/// Where a tag goes on a figure: by the tag tree when it has placed it
/// (`ancestors`, nearest first), else by its words. `None` for a tag that
/// doesn't describe a person.
pub fn slot_of<S: AsRef<str>>(name: &str, ancestors: &[S]) -> Option<Slot> {
    slot_in_tree(ancestors).unwrap_or_else(|| slot_by_name(name))
}

/// Words of a `name_(…)` suffix that mean an outfit or a version of a
/// character rather than their series.
const NOT_SERIES: &[&str] = &[
    "costume",
    "outfit",
    "uniform",
    "swimsuit",
    "casual",
    "work",
    "streetwear",
    "character",
    "cosplay",
    "male",
    "female",
    "young",
    "old",
    "child",
    "adult",
    "alternate",
    "alter",
    "ego",
    "append",
    "full",
    "cat",
    "bird",
    "ghost",
    "vampire",
    "summer",
    "winter",
    "school",
    "sailor",
    "lolita",
    "shirt",
    "dress",
    "kimono",
    "vtuber",
    "utaite",
    "human",
    "animal",
];

/// The series a Danbooru-style character name says it comes from, such as
/// `genshin_impact` for `lumine_(genshin_impact)`. Only a hint, for when
/// the collection doesn't know the character.
pub fn franchise_hint(character: &str) -> Option<String> {
    let (_, rest) = character.rsplit_once("_(")?;
    let series = rest.strip_suffix(')')?;
    // `1st_costume`, `casual`: a version of the character.
    let version = series.split('_').any(|w| {
        NOT_SERIES.contains(&w) || (w.len() >= 3 && w.starts_with(|c: char| c.is_ascii_digit()))
    });
    if series.is_empty() || version || series.contains(['(', ')']) {
        return None;
    }
    normalize_tag(series).ok()
}

/// `POST /posts/{id}/find-people`: who is in a post's picture. Nothing is
/// saved; `PUT /posts/{id}/figures` saves what the user keeps.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FoundPeople {
    /// One figure per person found, largest face first, each with its face
    /// box. Its `face_url` is a `data:image/jpeg;base64,…` URL of the crop
    /// saving it would keep, as nothing is stored yet.
    pub figures: Vec<Figure>,
    /// The model that found them.
    pub model: String,
}

/// The prompt asking an AI provider about the people in a picture. With
/// `faces` (found by the server, in thousandths of the picture) it asks for
/// one figure per face; without, for the faces too.
pub fn people_prompt(faces: &[FaceBox]) -> String {
    let shape = "{\"figures\": [{\"box\": 1, \"kind\": \"girl\", \"character\": \"\", \
\"cosplay\": \"\", \"franchise\": \"\", \"head\": [\"hat\"], \"body\": [\"dress\"], \"bottom\": [], \
\"shoes\": [\"boots\"], \"wear\": [], \"shape\": [], \"eyes\": [\"blue_eyes\"], \"skin\": [], \
\"hair\": [\"long_hair\"], \"pose\": [\"standing\"], \"other\": [\"smile\"]}]}";
    let rules = "Kind is girl, boy or other. The lists are that person's tags in danbooru \
conventions (lowercase English, words joined by underscores): head is headwear, body is tops and \
dresses, bottom is skirts, trousers and legwear, wear is other clothing and accessories, shape is \
their build, other is expression and the rest. Character is who they are as a danbooru character \
tag, cosplay the character a real person is dressed as, franchise the series the character comes \
from; leave them empty unless you are sure.";
    if faces.is_empty() {
        return format!(
            "Find each person's face in this image and describe the person. Answer with JSON \
only, no prose, in this shape: {} where \"face\" is [x, y, width, height] of the face in \
thousandths of the image's width and height from its top-left corner. {rules} \
Use an empty list when there is nobody.",
            shape.replace("\"box\": 1", "\"face\": [400, 100, 120, 160]")
        );
    }
    let boxes: Vec<String> = faces
        .iter()
        .enumerate()
        .map(|(i, f)| format!("{}: [{}, {}, {}, {}]", i + 1, f.x, f.y, f.w, f.h))
        .collect();
    format!(
        "These boxes mark faces in this image, as [x, y, width, height] in thousandths of the \
image's width and height from its top-left corner: {}. Describe the person each box belongs to. \
Answer with JSON only, no prose, in this shape: {shape} where \"box\" is the box's number. {rules} \
Leave out a box that holds no person's face.",
        boxes.join("; ")
    )
}

/// A face given as `[x, y, w, h]` in thousandths.
fn face_of(value: &serde_json::Value) -> Option<FaceBox> {
    let edges = value.as_array()?;
    let edge = |i: usize| {
        edges
            .get(i)?
            .as_f64()
            .filter(|v| v.is_finite())
            .map(|v| v.round().clamp(0.0, f64::from(FACE_SCALE)) as u16)
    };
    let face = FaceBox {
        x: edge(0)?,
        y: edge(1)?,
        w: edge(2)?,
        h: edge(3)?,
    };
    face.is_valid().then_some(face)
}

/// Reads a provider's answer to [`people_prompt`]: one figure per box it
/// describes, with that box as its face, or per face it gives when there
/// were no boxes. Each box is used once.
pub fn parse_people(answer: &str, faces: &[FaceBox]) -> Vec<Figure> {
    let Some(value) = crate::ai::json_object(answer) else {
        return Vec::new();
    };
    let Some(list) = value.get("figures").and_then(|f| f.as_array()) else {
        return Vec::new();
    };
    let mut used: Vec<FaceBox> = Vec::new();
    let mut out = Vec::new();
    for item in list.iter().take(MAX_FIGURES.saturating_mul(2)) {
        let face = if faces.is_empty() {
            item.get("face").and_then(face_of)
        } else {
            item.get("box")
                .and_then(|b| b.as_u64())
                .and_then(|n| usize::try_from(n).ok()?.checked_sub(1))
                .and_then(|i| faces.get(i).copied())
        };
        let Some(face) = face.filter(|f| !used.contains(f)) else {
            continue;
        };
        let Some(mut figure) = crate::ai::model_figure(item) else {
            continue;
        };
        figure.face = Some(face);
        used.push(face);
        out.push(figure);
        if out.len() == MAX_FIGURES {
            break;
        }
    }
    clean_figures(out).unwrap_or_default()
}

/// The franchise for each character the people found are, by name: what
/// the collection knows, else the name's hint when the collection has that
/// series or it reads like one. `known` maps characters to the franchise
/// the collection gives them; `series` maps hints to the tag the collection
/// has for them.
pub fn franchises(
    figures: &mut [Figure],
    known: &HashMap<String, String>,
    series: &HashMap<String, String>,
) {
    for figure in figures.iter_mut().filter(|f| f.franchise.is_none()) {
        let Some(who) = figure.cosplay.clone().or_else(|| figure.character.clone()) else {
            continue;
        };
        figure.franchise = known.get(&who).cloned().or_else(|| {
            let hint = franchise_hint(&who)?;
            Some(series.get(&hint).cloned().unwrap_or(hint))
        });
    }
}

/// Each hint the people found give, with the names the collection may
/// have it under (`fate` may be `fate_(series)`).
pub fn series_names(figures: &[Figure]) -> Vec<String> {
    let mut out = Vec::new();
    for figure in figures.iter().filter(|f| f.franchise.is_none()) {
        let who = figure.cosplay.as_ref().or(figure.character.as_ref());
        if let Some(hint) = who.and_then(|w| franchise_hint(w)) {
            out.push(format!("{hint}_(series)"));
            out.push(hint);
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vocab::VOCAB;

    fn tag(name: &str, score: f32) -> FoundTag {
        FoundTag {
            name: name.into(),
            category: None,
            score,
        }
    }

    fn character(name: &str, score: f32) -> FoundTag {
        FoundTag {
            category: Some(TagCategory::Character),
            ..tag(name, score)
        }
    }

    fn face(x: u16, y: u16, w: u16, h: u16) -> FaceBox {
        FaceBox { x, y, w, h }
    }

    #[test]
    fn letterboxing_scales_the_longer_side_and_back() {
        let lb = Letterbox::new(1600, 900, 640);
        assert_eq!((lb.width, lb.height), (640, 360));
        let back = lb.to_picture(Detection::centred(320.0, 180.0, 64.0, 32.0, 0.9));
        assert!((back.x - 720.0).abs() < 0.01, "{back:?}");
        assert!((back.y - 410.0).abs() < 0.01);
        assert!((back.w - 160.0).abs() < 0.01);
        assert!((back.h - 80.0).abs() < 0.01);
        assert_eq!(back.score, 0.9);
        // Tall, tiny and empty pictures still fit.
        let tall = Letterbox::new(100, 400, 320);
        assert_eq!((tall.width, tall.height), (80, 320));
        let tiny = Letterbox::new(1, 5000, 640);
        assert_eq!((tiny.width, tiny.height), (1, 640));
        let empty = Letterbox::new(0, 0, 0);
        assert!(empty.scale.is_finite() && empty.width >= 1);
    }

    #[test]
    fn suppression_keeps_the_best_of_each_face() {
        let a = Detection {
            x: 10.0,
            y: 10.0,
            w: 100.0,
            h: 100.0,
            score: 0.9,
        };
        let shifted = Detection {
            x: 20.0,
            score: 0.8,
            ..a
        };
        let inner = Detection {
            x: 30.0,
            y: 30.0,
            w: 40.0,
            h: 40.0,
            score: 0.95,
        };
        let other = Detection { x: 300.0, ..a };
        let broken = Detection {
            w: f32::NAN,
            score: 1.0,
            ..a
        };
        let kept = suppress(vec![shifted, a, other, broken], 0.3, 0.6);
        assert_eq!(kept, [a, other]);
        // A small box inside a big one is the same face, whichever wins.
        let kept = suppress(vec![a, inner], 0.3, 0.6);
        assert_eq!(kept, [inner]);
        assert!((a.iou(&shifted) - 90.0 / 110.0).abs() < 0.001);
        assert_eq!(a.iom(&inner), 1.0);
        assert_eq!(a.iou(&other), 0.0);
        assert!(suppress(Vec::new(), 0.3, 0.6).is_empty());
    }

    #[test]
    fn face_boxes_are_cut_to_the_picture() {
        let d = Detection {
            x: 100.0,
            y: 50.0,
            w: 200.0,
            h: 100.0,
            score: 1.0,
        };
        assert_eq!(face_box(d, 1000, 500), Some(face(100, 100, 200, 200)));
        let over = Detection {
            x: -50.0,
            y: 400.0,
            ..d
        };
        assert_eq!(face_box(over, 1000, 500), Some(face(0, 800, 150, 200)));
        let outside = Detection { x: 2000.0, ..d };
        assert_eq!(face_box(outside, 1000, 500), None);
        assert_eq!(face_box(d, 0, 500), None);
        let nan = Detection { y: f32::NAN, ..d };
        assert_eq!(face_box(nan, 1000, 500), None);
    }

    #[test]
    fn crops_pad_the_face_and_stay_inside() {
        // A 100 px face in the middle of a 1000 px square.
        let middle = face(450, 450, 100, 100);
        assert_eq!(
            face_crop(middle, 1000, 1000),
            Some(Rect {
                x: 425,
                y: 425,
                w: 150,
                h: 150
            })
        );
        assert_eq!(
            body_crop(middle, &[], 1000, 1000),
            Some(Rect {
                x: 300,
                y: 410,
                w: 400,
                h: 590
            })
        );
        // In a group: halfway to the neighbour on the right, down to the
        // head of the one in front; the far one and the face itself don't
        // count.
        let group = [
            middle,
            face(600, 450, 100, 100),
            face(450, 700, 100, 100),
            face(900, 100, 50, 50),
        ];
        assert_eq!(
            body_crop(middle, &group, 1000, 1000),
            Some(Rect {
                x: 300,
                y: 410,
                w: 275,
                h: 290
            })
        );
        // Never narrower than the face, however close the neighbour.
        let close = [face(455, 450, 100, 100)];
        let crop = body_crop(middle, &close, 1000, 1000).unwrap();
        assert!(crop.x <= 450 && crop.x + crop.w >= 550, "{crop:?}");
        // At a corner the crop is cut, not moved.
        let corner = face(0, 0, 100, 100);
        assert_eq!(
            face_crop(corner, 1000, 1000),
            Some(Rect {
                x: 0,
                y: 0,
                w: 125,
                h: 125
            })
        );
        // Whatever the box, the crop is a pixel or more inside the picture.
        for (w, h) in [(1, 1), (3, 7000), (7000, 3)] {
            for f in [face(999, 999, 1, 1), face(0, 0, 1000, 1000)] {
                for crop in [face_crop(f, w, h), body_crop(f, &[f], w, h)] {
                    let crop = crop.unwrap();
                    assert!(crop.w >= 1 && crop.h >= 1);
                    assert!(crop.x + crop.w <= w && crop.y + crop.h <= h, "{crop:?}");
                }
            }
        }
        assert_eq!(face_crop(middle, 0, 10), None);
    }

    fn vocab_ancestors(name: &str) -> Vec<&'static str> {
        let mut out = Vec::new();
        let mut at = name;
        while let Some((_, parent)) = VOCAB.iter().find(|(t, _)| *t == at) {
            out.push(*parent);
            at = parent;
        }
        out
    }

    #[test]
    fn the_starter_tree_puts_tags_in_slots() {
        let slot = |name: &str| slot_of(name, &vocab_ancestors(name));
        assert_eq!(slot("witch_hat"), Some(Slot::Head));
        assert_eq!(slot("hair_ornament"), Some(Slot::Head));
        assert_eq!(slot("serafuku"), Some(Slot::Body));
        assert_eq!(slot("bikini"), Some(Slot::Body));
        assert_eq!(slot("pleated_skirt"), Some(Slot::Bottom));
        assert_eq!(slot("thighhighs"), Some(Slot::Bottom));
        assert_eq!(slot("boots"), Some(Slot::Shoes));
        assert_eq!(slot("sunglasses"), Some(Slot::Wear));
        assert_eq!(slot("petite"), Some(Slot::Shape));
        assert_eq!(slot("cat_ears"), Some(Slot::Other));
        assert_eq!(slot("blue_eyes"), Some(Slot::Eyes));
        assert_eq!(slot("freckles"), Some(Slot::Skin));
        assert_eq!(slot("twintails"), Some(Slot::Hair));
        assert_eq!(slot("smile"), Some(Slot::Other));
        assert_eq!(slot("sitting"), Some(Slot::Pose));
        // About the picture, not a person.
        assert_eq!(slot("1girl"), None);
        assert_eq!(slot("outdoors"), None);
        assert_eq!(slot("greyscale"), None);
        // Every tag of a section that describes people has a slot.
        for (tag, _) in VOCAB {
            let top = vocab_ancestors(tag).last().copied();
            if matches!(top, Some("hair" | "eyes" | "skin" | "clothing")) {
                assert!(slot(tag).is_some(), "{tag}");
            }
        }
    }

    #[test]
    fn the_admins_tree_wins_over_the_words() {
        assert_eq!(slot_of("white_shirt", &["setting"]), None);
        assert_eq!(slot_of("white_shirt", &["my_things", "style"]), None);
        // The admin's own sections say nothing.
        assert_eq!(slot_of("white_shirt", &["my_things"]), Some(Slot::Body));
        assert_eq!(
            slot_of("white_shirt", &["headwear", "clothing"]),
            Some(Slot::Head)
        );
        assert_eq!(slot_of::<&str>("white_shirt", &[]), Some(Slot::Body));
    }

    #[test]
    fn words_place_what_the_tree_lacks() {
        assert_eq!(slot_by_name("white_shirt"), Some(Slot::Body));
        assert_eq!(slot_by_name("long_sleeves"), Some(Slot::Body));
        assert_eq!(slot_by_name("black_pantyhose"), Some(Slot::Bottom));
        assert_eq!(slot_by_name("hair_between_eyes"), Some(Slot::Hair));
        assert_eq!(slot_by_name("hair_ribbon"), Some(Slot::Head));
        assert_eq!(slot_by_name("x_hair_ornament"), Some(Slot::Head));
        assert_eq!(slot_by_name("short_hair"), Some(Slot::Hair));
        assert_eq!(slot_by_name("large_breasts"), Some(Slot::Shape));
        assert_eq!(slot_by_name("red_necktie"), Some(Slot::Wear));
        assert_eq!(slot_by_name("open_mouth"), Some(Slot::Other));
        assert_eq!(slot_by_name("simple_background"), None);
        assert_eq!(slot_by_name("_"), None);
        assert_eq!(slot_by_name(""), None);
    }

    /// One person alone in a picture.
    fn one(
        p: &FoundPerson,
        on_post: impl Fn(&str) -> bool,
        place: impl Fn(&str) -> Option<Slot>,
    ) -> Figure {
        let mut figures = figures_from(std::slice::from_ref(p), on_post, place);
        assert_eq!(figures.len(), 1);
        figures.remove(0)
    }

    fn person(face_tags: Vec<FoundTag>, body_tags: Vec<FoundTag>) -> FoundPerson {
        FoundPerson {
            face: face(100, 100, 200, 200),
            score: 0.9,
            face_tags,
            body_tags,
        }
    }

    #[test]
    fn a_person_becomes_a_figure() {
        let p = person(
            vec![
                tag("1girl", 0.97),
                tag("solo", 0.9),
                tag("long_hair", 0.8),
                tag("blue_eyes", 0.7),
                tag("smile", 0.45),
                tag("upper_body", 0.9),
                tag("white_shirt", 0.9),
                character("hatsune_miku", 0.95),
            ],
            vec![
                tag("1girl", 0.9),
                tag("white_shirt", 0.3),
                tag("pleated_skirt", 0.6),
                tag("long_hair", 0.2),
                tag("blue_eyes", 0.9),
                tag("outdoors", 0.9),
            ],
        );
        let f = one(&p, |_| false, |n| slot_of::<&str>(n, &[]));
        assert_eq!(f.kind, FigureKind::Girl);
        assert_eq!(f.character.as_deref(), Some("hatsune_miku"));
        assert_eq!(f.cosplay, None);
        assert_eq!(f.face, Some(p.face));
        assert_eq!(f.tags[&Slot::Hair], ["long_hair"]);
        assert_eq!(f.tags[&Slot::Eyes], ["blue_eyes"]);
        assert_eq!(f.tags[&Slot::Bottom], ["pleated_skirt"]);
        // Clothes count on the body's crop, looks on the face's; framing
        // and weak readings stay off.
        assert!(!f.tags.contains_key(&Slot::Body));
        assert!(!f.tags.contains_key(&Slot::Other));
        assert_eq!(f.franchise, None);
    }

    #[test]
    fn unsure_readings_leave_fields_empty() {
        let p = person(
            vec![
                tag("1girl", 0.4),
                tag("1boy", 0.45),
                character("hatsune_miku", 0.8),
            ],
            Vec::new(),
        );
        let f = one(&p, |_| false, |_| Some(Slot::Other));
        assert_eq!(f.kind, FigureKind::Other);
        assert_eq!(f.character, None);
        assert!(f.tags.is_empty());
        let boy = person(vec![tag("1girl", 0.55), tag("male_focus", 0.8)], Vec::new());
        assert_eq!(one(&boy, |_| false, |_| None).kind, FigureKind::Boy);
    }

    #[test]
    fn the_kind_comes_from_the_face() {
        // A neighbour in the body's crop doesn't count.
        let p = person(
            vec![tag("1boy", 0.98)],
            vec![tag("1girl", 0.97), tag("1boy", 0.9)],
        );
        assert_eq!(one(&p, |_| false, |_| None).kind, FigureKind::Boy);
        // On a photograph only girls are trusted, however it shows.
        let man = person(vec![tag("1boy", 0.97), tag("realistic", 0.9)], Vec::new());
        assert_eq!(one(&man, |_| false, |_| None).kind, FigureKind::Other);
        let masked = person(vec![tag("1boy", 0.97)], vec![tag("realistic", 0.6)]);
        assert_eq!(one(&masked, |_| false, |_| None).kind, FigureKind::Other);
        let masked = person(vec![tag("1boy", 0.97)], Vec::new());
        let on_post = |t: &str| t == "photorealistic";
        assert_eq!(one(&masked, on_post, |_| None).kind, FigureKind::Other);
        // In a group, half the faces reading as a photo make it one.
        let kinds = |group: &[FoundPerson]| -> Vec<FigureKind> {
            figures_from(group, |_| false, |_| None)
                .into_iter()
                .map(|f| f.kind)
                .collect()
        };
        assert_eq!(
            kinds(&[masked.clone(), man.clone()]),
            [FigureKind::Other; 2]
        );
        // A drawing with a face that reads as a photo stays a drawing.
        let mostly_drawn = [masked.clone(), masked.clone(), man];
        assert_eq!(kinds(&mostly_drawn), [FigureKind::Boy; 3]);
        assert!(kinds(&[]).is_empty());
        let woman = person(vec![tag("1girl", 0.9), tag("realistic", 0.9)], Vec::new());
        assert_eq!(one(&woman, |_| false, |_| None).kind, FigureKind::Girl);
        // A character known only from the body's crop may be the neighbour.
        let p = person(
            vec![tag("1girl", 0.9)],
            vec![character("hatsune_miku", 0.99)],
        );
        assert_eq!(one(&p, |_| false, |_| None).character, None);
    }

    #[test]
    fn a_character_needs_a_sure_or_a_second_reading() {
        let face_only = person(vec![character("luigi", 0.91)], Vec::new());
        assert_eq!(one(&face_only, |_| false, |_| None).character, None);
        let f = one(&face_only, |n| n == "luigi", |_| None);
        assert_eq!(f.character.as_deref(), Some("luigi"));
        let both = person(
            vec![character("misty_(pokemon)", 0.93)],
            vec![character("misty_(pokemon)", 0.96)],
        );
        let f = one(&both, |_| false, |_| None);
        assert_eq!(f.character.as_deref(), Some("misty_(pokemon)"));
        // The body's crop alone may be a neighbour.
        let body_only = person(Vec::new(), vec![character("luigi", 0.99)]);
        assert_eq!(one(&body_only, |_| true, |_| None).character, None);
    }

    #[test]
    fn a_character_in_a_photograph_is_a_cosplay() {
        let p = person(
            vec![
                tag("1girl", 0.9),
                tag("realistic", 0.8),
                character("hatsune_miku", 0.9),
            ],
            Vec::new(),
        );
        let f = one(&p, |_| true, |_| None);
        assert_eq!(f.character, None);
        assert_eq!(f.cosplay.as_deref(), Some("hatsune_miku"));
        let p = person(
            vec![character("saber_(fate)", 0.96)],
            vec![tag("cosplay", 0.5)],
        );
        assert_eq!(
            one(&p, |_| false, |_| None).cosplay.as_deref(),
            Some("saber_(fate)")
        );
    }

    #[test]
    fn a_person_needs_a_person_tag_or_a_strong_solo() {
        assert!(is_person(&[tag("1boy", 0.6), tag("sky", 0.99)]));
        assert!(is_person(&[tag("1girl", 0.42), tag("solo", 0.93)]));
        assert!(!is_person(&[tag("1girl", 0.43), tag("solo", 0.67)]));
        assert!(!is_person(&[tag("solo", 0.79), tag("no_humans", 0.8)]));
        assert!(!is_person(&[tag("1boy", 0.9), tag("no_humans", 0.6)]));
        assert!(!is_person(&[]));
    }

    #[test]
    fn suffixes_hint_at_the_series() {
        assert_eq!(
            franchise_hint("lumine_(genshin_impact)").as_deref(),
            Some("genshin_impact")
        );
        assert_eq!(franchise_hint("saber_(fate)").as_deref(), Some("fate"));
        assert_eq!(franchise_hint("rem_(re:zero)").as_deref(), Some("re:zero"));
        assert_eq!(franchise_hint("hatsune_miku"), None);
        for outfit in [
            "marine_(1st_costume)",
            "kaga_(casual)",
            "miku_(append)",
            "x_(swimsuit_of_perpetual_summer)",
            "y_(young)",
            "z_(3rd_costume)",
            "w_()",
        ] {
            assert_eq!(franchise_hint(outfit), None, "{outfit}");
        }
    }

    #[test]
    fn franchises_come_from_the_collection_first() {
        let mut figures = vec![
            Figure {
                character: Some("lumine_(genshin_impact)".into()),
                ..Figure::default()
            },
            Figure {
                cosplay: Some("saber_(fate)".into()),
                ..Figure::default()
            },
            Figure {
                character: Some("hatsune_miku".into()),
                ..Figure::default()
            },
            Figure {
                character: Some("rem_(re:zero)".into()),
                franchise: Some("given".into()),
                ..Figure::default()
            },
        ];
        assert_eq!(
            series_names(&figures),
            [
                "fate",
                "fate_(series)",
                "genshin_impact",
                "genshin_impact_(series)"
            ]
        );
        let known = HashMap::from([("hatsune_miku".to_owned(), "vocaloid".to_owned())]);
        let series = HashMap::from([("fate".to_owned(), "fate_(series)".to_owned())]);
        franchises(&mut figures, &known, &series);
        let got: Vec<_> = figures.iter().map(|f| f.franchise.as_deref()).collect();
        assert_eq!(
            got,
            [
                Some("genshin_impact"),
                Some("fate_(series)"),
                Some("vocaloid"),
                Some("given")
            ]
        );
    }

    #[test]
    fn the_prompt_lists_the_boxes() {
        let prompt = people_prompt(&[face(1, 2, 3, 4), face(500, 0, 100, 100)]);
        assert!(
            prompt.contains("1: [1, 2, 3, 4]; 2: [500, 0, 100, 100]"),
            "{prompt}"
        );
        assert!(prompt.contains("\"box\": 1"));
        let free = people_prompt(&[]);
        assert!(free.contains("\"face\": [400, 100, 120, 160]"), "{free}");
        assert!(!free.contains("\"box\""));
    }

    #[test]
    fn answers_give_one_figure_per_box() {
        let faces = [face(0, 0, 100, 100), face(500, 0, 100, 100)];
        let answer = r#"Sure! ```json
            {"figures": [
              {"box": 2, "kind": "boy", "hair": ["short_hair"], "character": "x"},
              {"box": 2, "kind": "girl"},
              {"box": 9, "kind": "girl"},
              {"kind": "girl"},
              {"box": 1, "kind": "other", "franchise": "some_series"}
            ]}```"#;
        let figures = parse_people(answer, &faces);
        assert_eq!(figures.len(), 2);
        assert_eq!(figures[0].face, Some(faces[1]));
        assert_eq!(figures[0].kind, FigureKind::Boy);
        assert_eq!(figures[0].tags[&Slot::Hair], ["short_hair"]);
        assert_eq!(figures[1].face, Some(faces[0]));
        assert_eq!(figures[1].franchise.as_deref(), Some("some_series"));
        assert!(parse_people("no idea", &faces).is_empty());
        assert!(parse_people(r#"{"figures": 3}"#, &faces).is_empty());
    }

    #[test]
    fn answers_without_boxes_give_their_own_faces() {
        let answer = r#"{"figures": [
            {"face": [100, 200, 50, 60.4], "kind": "girl"},
            {"face": [990, 0, 50, 50], "kind": "girl"},
            {"face": [1, 2], "kind": "boy"},
            {"face": "left", "kind": "boy"}
        ]}"#;
        let figures = parse_people(answer, &[]);
        assert_eq!(figures.len(), 1);
        assert_eq!(figures[0].face, Some(face(100, 200, 50, 60)));
    }
}
