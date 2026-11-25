//! What the server and its AI worker say to each other: one JSON object per
//! line over the worker's stdin and stdout. Images travel base64-encoded.

use serde::{Deserialize, Serialize};

use crate::figure::FaceBox;
use crate::model::{Rating, TagCategory};

/// The worker's error when a model's files on disk don't load. The server then
/// checks them against their hashes and downloads the damaged ones again.
pub const DAMAGED_MODEL: &str = "a model file is damaged";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WorkerRequest {
    /// Checks that the runtime loads.
    Hello,
    /// Times the built-in calibration models.
    Calibrate,
    /// Times a model with a generated picture or sentence, on the processor
    /// and on the graphics card when there is one.
    Measure { model: String },
    Tag {
        model: String,
        gpu: bool,
        jpeg: String,
    },
    Translate {
        model: String,
        gpu: bool,
        /// Language codes, primary subtag.
        from: String,
        to: String,
        texts: Vec<String>,
    },
    /// Finds text in a comic page.
    Detect {
        detector: String,
        gpu: bool,
        jpeg: String,
    },
    /// Reads Japanese text in parts of a page.
    Read {
        gpu: bool,
        jpeg: String,
        boxes: Vec<TextBox>,
    },
    /// Finds the faces in a picture and reads each person with the model's
    /// tagger.
    People {
        model: String,
        gpu: bool,
        jpeg: String,
    },
    /// Only finds the faces, for an AI provider to describe.
    Faces { gpu: bool, jpeg: String },
    /// Frees all models.
    Unload,
}

/// A piece of text on a page, in pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextBox {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// One tag a tagger found.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FoundTag {
    pub name: String,
    #[serde(default)]
    pub category: Option<TagCategory>,
    pub score: f32,
}

/// A face a face finder found.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FoundFace {
    pub face: FaceBox,
    pub score: f32,
}

/// A person found by their face, with what the tagger read off a crop of
/// the face and one of the body below it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FoundPerson {
    pub face: FaceBox,
    pub score: f32,
    pub face_tags: Vec<FoundTag>,
    pub body_tags: Vec<FoundTag>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum WorkerReply {
    Hello {
        /// The graphics card interface this build has and that works here
        /// (`cuda`, `webgpu`).
        gpu: Option<String>,
    },
    Calibrated {
        float_ms: f64,
        int8_ms: f64,
        gpu_ms: Option<f64>,
    },
    Measured {
        cpu_ms: u32,
        gpu_ms: Option<u32>,
        /// The graphics card gave the same results as the processor.
        gpu_matches: bool,
    },
    Tags {
        tags: Vec<FoundTag>,
        rating: Option<Rating>,
    },
    Translated {
        texts: Vec<String>,
    },
    Boxes {
        boxes: Vec<TextBox>,
    },
    Texts {
        texts: Vec<String>,
    },
    People {
        people: Vec<FoundPerson>,
    },
    Faces {
        faces: Vec<FoundFace>,
    },
    Done,
    Error {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_replies_keep_their_shape() {
        let req = WorkerRequest::Tag {
            model: "wd-eva02".into(),
            gpu: false,
            jpeg: "AA==".into(),
        };
        let text = serde_json::to_string(&req).unwrap();
        assert!(text.starts_with(r#"{"op":"tag""#), "{text}");
        assert_eq!(serde_json::from_str::<WorkerRequest>(&text).unwrap(), req);
        let reply = WorkerReply::Tags {
            tags: vec![FoundTag {
                name: "1girl".into(),
                category: Some(TagCategory::General),
                score: 0.9,
            }],
            rating: Some(Rating::Safe),
        };
        let text = serde_json::to_string(&reply).unwrap();
        assert_eq!(serde_json::from_str::<WorkerReply>(&text).unwrap(), reply);
        assert!(serde_json::from_str::<WorkerReply>(r#"{"reply":"nope"}"#).is_err());
    }
}
