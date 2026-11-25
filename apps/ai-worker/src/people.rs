//! Finding the people in a picture: faces first, with one detector for
//! photographs and one for drawings, then what the tagger reads off each
//! face and off the body below it.

use image::RgbImage;
use ort::inputs;
use starbooru_core::ai_catalog::{self, Engine};
use starbooru_core::ai_worker::{FoundFace, FoundPerson};
use starbooru_core::figure::MAX_FIGURES;
use starbooru_core::people_ai::{self, Detection};

use crate::onnx;
use crate::picture;
use crate::runtime::Runtime;
use crate::tag;

const YUNET: &str = "yunet";
const YUNET_FILE: &str = "face_detection_yunet_2023mar.onnx";
const ANIME: &str = "anime-faces";
const ANIME_FILE: &str = "model.onnx";
/// Both detectors take a 640 px square.
const INPUT: u32 = 640;
/// YuNet misses faces larger than a third of its input, so it also looks at
/// the picture at half the size.
const YUNET_FITS: [u32; 2] = [640, 320];
const YUNET_STRIDES: [u32; 3] = [8, 16, 32];
const BGR: [usize; 3] = [2, 1, 0];
const RGB: [usize; 3] = [0, 1, 2];
/// The YOLO detector's padding grey.
const YOLO_FILL: u8 = 114;
/// Lowest scores kept, measured on public-domain photos and drawings: what
/// gets through and isn't a face, the tagger turns away.
const YUNET_MIN: f32 = 0.55;
const ANIME_MIN: f32 = 0.4;
/// Two boxes overlapping this much are one face.
const SAME_IOU: f32 = 0.3;
/// A box half inside another is the same face, found larger: the two
/// finders frame a face differently.
const SAME_IOM: f32 = 0.5;
/// Faces smaller than this, in pixels, are too small to read.
const MIN_FACE: f32 = 16.0;

/// A probability, whatever the model gave.
fn unit(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// YuNet's faces at one size.
fn yunet_at(
    rt: &mut Runtime,
    gpu: bool,
    image: &RgbImage,
    fit: u32,
) -> Result<Vec<Detection>, String> {
    let path = rt.file(YUNET, YUNET_FILE)?;
    let (lb, shape, data) = picture::letterboxed_chw(image, fit, INPUT, 0, BGR, 1.0);
    let session = rt.session(&path, gpu)?;
    let outputs = session
        .run(inputs!["input" => onnx::floats(&shape, data)?])
        .map_err(|e| e.to_string())?;
    let mut found = Vec::new();
    for stride in YUNET_STRIDES {
        let get = |name: String| outputs.get(&name).ok_or("unexpected model output");
        let (_, cls) = onnx::read_floats(get(format!("cls_{stride}"))?)?;
        let (_, obj) = onnx::read_floats(get(format!("obj_{stride}"))?)?;
        let (_, bbox) = onnx::read_floats(get(format!("bbox_{stride}"))?)?;
        let cols = (INPUT / stride) as usize;
        let s = stride as f32;
        let cells = cls.iter().zip(obj).zip(bbox.as_chunks::<4>().0);
        for (i, ((&c, &o), &[dx, dy, dw, dh])) in cells.enumerate() {
            let score = (unit(c) * unit(o)).sqrt();
            if score < YUNET_MIN {
                continue;
            }
            let (row, col) = ((i / cols) as f32, (i % cols) as f32);
            let d = Detection::centred(
                (col + dx) * s,
                (row + dy) * s,
                dw.min(10.0).exp() * s,
                dh.min(10.0).exp() * s,
                score,
            );
            found.push(lb.to_picture(d));
        }
    }
    Ok(found)
}

/// The drawn faces the YOLO detector finds.
fn anime(rt: &mut Runtime, gpu: bool, image: &RgbImage) -> Result<Vec<Detection>, String> {
    let path = rt.file(ANIME, ANIME_FILE)?;
    let (lb, shape, data) =
        picture::letterboxed_chw(image, INPUT, INPUT, YOLO_FILL, RGB, 1.0 / 255.0);
    let session = rt.session(&path, gpu)?;
    let outputs = session
        .run(inputs!["images" => onnx::floats(&shape, data)?])
        .map_err(|e| e.to_string())?;
    let value = outputs.get("output0").ok_or("unexpected model output")?;
    let (shape, rows) = onnx::read_floats(value)?;
    // [1, 5, anchors]: centre x, centre y, width, height, score.
    let anchors = shape
        .last()
        .and_then(|&n| usize::try_from(n).ok())
        .filter(|&n| shape.get(1) == Some(&5) && n > 0 && rows.len() >= n.saturating_mul(5))
        .ok_or("unexpected model output")?;
    let row = |k: usize| rows.get(k * anchors..(k + 1) * anchors).unwrap_or(&[]);
    let (cx, cy, w, h, score) = (row(0), row(1), row(2), row(3), row(4));
    let mut found = Vec::new();
    for (i, &s) in score.iter().enumerate() {
        if s < ANIME_MIN {
            continue;
        }
        let at = |v: &[f32]| v.get(i).copied().unwrap_or(f32::NAN);
        found.push(lb.to_picture(Detection::centred(at(cx), at(cy), at(w), at(h), s)));
    }
    Ok(found)
}

/// The faces in a picture, largest first: twice [`MAX_FIGURES`] at most, as
/// the tagger turns some away.
pub fn faces(rt: &mut Runtime, gpu: bool, image: &RgbImage) -> Result<Vec<FoundFace>, String> {
    let mut found = Vec::new();
    for fit in YUNET_FITS {
        found.extend(yunet_at(rt, gpu, image, fit)?);
    }
    found.extend(anime(rt, gpu, image)?);
    let mut kept: Vec<Detection> = people_ai::suppress(found, SAME_IOU, SAME_IOM)
        .into_iter()
        .filter(|d| d.w.min(d.h) >= MIN_FACE)
        .collect();
    kept.sort_by(|a, b| (b.w * b.h).total_cmp(&(a.w * a.h)));
    let (width, height) = image.dimensions();
    Ok(kept
        .into_iter()
        .filter_map(|d| {
            Some(FoundFace {
                face: people_ai::face_box(d, width, height)?,
                score: d.score,
            })
        })
        .take(MAX_FIGURES.saturating_mul(2))
        .collect())
}

/// The people in a picture: each face the tagger agrees is a person's, with
/// what it read off the face and the body.
pub fn people(
    rt: &mut Runtime,
    model: &str,
    gpu: bool,
    image: &RgbImage,
) -> Result<Vec<FoundPerson>, String> {
    let entry = ai_catalog::model(model).ok_or("unknown model")?;
    let Engine::People { tagger } = entry.engine else {
        return Err("unknown model".into());
    };
    let (width, height) = image.dimensions();
    let found = faces(rt, gpu, image)?;
    let boxes: Vec<_> = found.iter().map(|f| f.face).collect();
    let mut out = Vec::new();
    for found in found {
        if out.len() == MAX_FIGURES {
            break;
        }
        let Some(rect) = people_ai::face_crop(found.face, width, height) else {
            continue;
        };
        let (face_tags, _) = tag::tag(rt, tagger, gpu, &picture::crop(image, rect))?;
        if !people_ai::is_person(&face_tags) {
            continue;
        }
        let body_tags = match people_ai::body_crop(found.face, &boxes, width, height) {
            Some(rect) => tag::tag(rt, tagger, gpu, &picture::crop(image, rect))?.0,
            None => Vec::new(),
        };
        out.push(FoundPerson {
            face: found.face,
            score: found.score,
            face_tags,
            body_tags,
        });
    }
    Ok(out)
}
