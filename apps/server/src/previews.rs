//! Link previews: the Open Graph tags chats and sites read when someone
//! pastes a link (`api.md` › *Link previews*), worked out here without I/O.
//! `routes/previews.rs` reads the database and serves the pictures.
//!
//! The tags go in the page itself, since link-preview bots don't run the
//! app. Every client gets the same page: nothing here looks at the
//! User-Agent.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use axum::http::HeaderMap;
use starbooru_core::share::is_share_token;
use starbooru_core::{FORMATS, MediaKind, SAMPLE_SIZE};
use url::Url;

use crate::storage::thumb_side;
use crate::web::escape_html;

/// A preview fits in this square, so a big picture still makes Facebook's
/// large card (at least 600 × 315), which a thumbnail misses.
pub const PREVIEW_SIZE: u32 = 1200;
/// Pictures a share link's tags name: Discord shows them as a gallery, the
/// other apps take the first.
pub const MAX_SHARE_IMAGES: usize = 4;
/// A post's description stops at the last tag that fits in this.
const DESCRIPTION_CHARS: usize = 200;

/// What every share link's page carries, preview or not.
pub const ROBOTS: &str = "<meta name=\"robots\" content=\"noindex, nofollow\">";

/// The app's two languages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    It,
}

/// The language for the texts: the first of the app's languages in
/// `Accept-Language`, by preference, and English when it names neither.
pub fn language(accept: Option<&str>) -> Lang {
    let mut best: Option<(u32, bool)> = None;
    for part in accept.unwrap_or_default().split(',').take(32) {
        let mut pieces = part.split(';');
        let tag = pieces.next().unwrap_or_default().trim();
        let weight = pieces
            .find_map(|p| p.trim().strip_prefix("q="))
            .map_or(Some(1000), quality);
        let Some(weight) = weight.filter(|w| *w > 0) else {
            continue;
        };
        let primary = tag.split(['-', '_']).next().unwrap_or_default();
        let italian = primary.eq_ignore_ascii_case("it");
        if !italian && !primary.eq_ignore_ascii_case("en") {
            continue;
        }
        // The earlier of two equal weights wins.
        if best.is_none_or(|(w, _)| weight > w) {
            best = Some((weight, italian));
        }
    }
    match best {
        Some((_, true)) => Lang::It,
        _ => Lang::En,
    }
}

/// A `q=` value in thousandths; `None` when it isn't one.
fn quality(raw: &str) -> Option<u32> {
    let q: f64 = raw.trim().parse().ok()?;
    (0.0..=1.0)
        .contains(&q)
        .then(|| (q * 1000.0).round() as u32)
}

/// What an app page links to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// `/s/{token}`, with a well-formed token.
    Share(String),
    /// `/post/{id}`.
    Post(i64),
}

/// The link an app path is, if it is one the server previews.
pub fn link_of(path: &str) -> Option<Link> {
    if let Some(token) = path.strip_prefix("/s/") {
        return is_share_token(token).then(|| Link::Share(token.to_owned()));
    }
    let id = path.strip_prefix("/post/")?;
    if id.is_empty() || id.len() > 18 || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    id.parse().ok().filter(|id| *id > 0).map(Link::Post)
}

/// A tag as people write it: `blue_sky` reads `blue sky`.
pub fn prettify(tag: &str) -> String {
    tag.replace('_', " ")
}

/// How many of each kind a link holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub images: usize,
    pub animations: usize,
    pub videos: usize,
}

impl Counts {
    pub fn add(&mut self, kind: MediaKind) {
        let n = match kind {
            MediaKind::Image => &mut self.images,
            MediaKind::Animation => &mut self.animations,
            MediaKind::Video => &mut self.videos,
        };
        *n = n.saturating_add(1);
    }
}

/// "3 pictures and 1 video"; `None` for an empty link.
pub fn count_text(lang: Lang, counts: Counts) -> Option<String> {
    let words: [(usize, &str, &str); 3] = match lang {
        Lang::En => [
            (counts.images, "picture", "pictures"),
            (counts.animations, "animation", "animations"),
            (counts.videos, "video", "videos"),
        ],
        Lang::It => [
            (counts.images, "immagine", "immagini"),
            (counts.animations, "animazione", "animazioni"),
            (counts.videos, "video", "video"),
        ],
    };
    let parts: Vec<String> = words
        .iter()
        .filter(|(n, ..)| *n > 0)
        .map(|(n, one, many)| format!("{n} {}", if *n == 1 { one } else { many }))
        .collect();
    let and = match lang {
        Lang::En => " and ",
        Lang::It => " e ",
    };
    match parts.split_last() {
        None => None,
        Some((last, [])) => Some(last.clone()),
        Some((last, rest)) => Some(format!("{}{and}{last}", rest.join(", "))),
    }
}

/// A post's title: up to three of its characters, else of its copyrights,
/// else of its artists, else its number.
pub fn post_title(
    characters: &[String],
    copyrights: &[String],
    artists: &[String],
    id: i64,
) -> String {
    [characters, copyrights, artists]
        .into_iter()
        .find(|tags| !tags.is_empty())
        .map(|tags| {
            tags.iter()
                .take(3)
                .map(|t| prettify(t))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_else(|| format!("#{id}"))
}

/// A post's description: its general tags, as many as fit in
/// [`DESCRIPTION_CHARS`], or `fallback` when it has none.
pub fn post_description(general: &[String], fallback: &str) -> String {
    let mut out = String::new();
    let mut chars = 0usize;
    for tag in general {
        let tag = prettify(tag);
        let len = tag.chars().count();
        let sep = if out.is_empty() { 0 } else { 2 };
        if chars.saturating_add(sep).saturating_add(len) > DESCRIPTION_CHARS {
            if out.is_empty() {
                // One tag longer than the whole description.
                out = tag.chars().take(DESCRIPTION_CHARS).collect();
            }
            out.push('…');
            return out;
        }
        if sep > 0 {
            out.push_str(", ");
        }
        out.push_str(&tag);
        chars = chars.saturating_add(sep).saturating_add(len);
    }
    if out.is_empty() {
        fallback.to_owned()
    } else {
        out
    }
}

/// The picture a preview is made from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A big image's sample.
    Sample,
    /// The original, which `image` decodes (a GIF's first frame).
    Original,
    /// An AVIF or JPEG XL through ffmpeg, at the frame size thumbnails use.
    Frame,
    /// The thumbnail: videos, and AVIF or JPEG XL without ffmpeg.
    Thumb,
}

/// Where a post's preview comes from; `None` when it can't have one.
pub fn source(
    kind: MediaKind,
    ext: &str,
    has_sample: Option<bool>,
    has_thumb: bool,
    tools: bool,
) -> Option<Source> {
    if has_sample == Some(true) {
        return Some(Source::Sample);
    }
    let decodable = FORMATS.iter().any(|(_, e, _, d)| *e == ext && *d);
    match kind {
        MediaKind::Image | MediaKind::Animation if decodable => Some(Source::Original),
        MediaKind::Image if tools => Some(Source::Frame),
        _ => has_thumb.then_some(Source::Thumb),
    }
}

/// `width` × `height` scaled to fit in `side`², never enlarged, rounded as
/// the `image` crate rounds it, so a size worked out here is the one it
/// makes.
pub fn fit(width: u32, height: u32, side: u32) -> (u32, u32) {
    if width <= side && height <= side {
        return (width, height);
    }
    let ratio = f64::min(
        f64::from(side) / f64::from(width),
        f64::from(side) / f64::from(height),
    );
    let scale = |n: u32| ((f64::from(n) * ratio).round() as u32).max(1);
    (scale(width), scale(height))
}

/// The size of a post's preview, from its stored size and the source, so
/// the page names it without decoding anything. `None` without a size.
pub fn size(width: Option<i32>, height: Option<i32>, source: Source) -> Option<(u32, u32)> {
    let w = width
        .and_then(|w| u32::try_from(w).ok())
        .filter(|w| *w > 0)?;
    let h = height
        .and_then(|h| u32::try_from(h).ok())
        .filter(|h| *h > 0)?;
    let (w, h) = match source {
        Source::Sample => fit(w, h, SAMPLE_SIZE),
        Source::Original => (w, h),
        Source::Frame => fit(w, h, crate::probe::frame_size(width, height)),
        Source::Thumb => fit(w, h, thumb_side(w, h)),
    };
    Some(fit(w, h, PREVIEW_SIZE))
}

/// The first 8 hex digits of a post's SHA-256, for `?v=`: an edited
/// picture is another post, so its preview gets another address.
pub fn version(sha256: &[u8]) -> String {
    hex::encode(sha256.get(..4).unwrap_or_default())
}

/// The address the server is reached at, ending in `/`: `public` when it
/// is set, else the request's host. Behind a trusted proxy its
/// `X-Forwarded-Proto` and `X-Forwarded-Host` come first. Without either,
/// a local name or a private address is `http` and anything else `https`.
/// `None` when the host isn't a plain host name or address.
pub fn base_url(
    public: Option<&Url>,
    trust_proxy: bool,
    headers: &HeaderMap,
    authority: Option<&str>,
) -> Option<String> {
    if let Some(public) = public {
        return Some(public.as_str().to_owned());
    }
    let header = |name: &str| {
        headers
            .get_all(name)
            .iter()
            .next_back()
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let forwarded = |name| trust_proxy.then(|| header(name)).flatten();
    let raw = forwarded("x-forwarded-host")
        .or_else(|| header("host"))
        .or(authority)?;
    let (host, name) = valid_host(raw)?;
    let scheme = match forwarded("x-forwarded-proto").map(str::to_ascii_lowercase) {
        Some(proto) if proto == "http" || proto == "https" => proto,
        _ if is_local(&name) => "http".to_owned(),
        _ => "https".to_owned(),
    };
    Some(format!("{scheme}://{host}/"))
}

/// A host name or address with an optional port, lowercased, and the name
/// alone; `None` for anything else.
fn valid_host(raw: &str) -> Option<(String, String)> {
    let raw = raw.trim().to_ascii_lowercase();
    if raw.is_empty() || raw.len() > 261 {
        return None;
    }
    let (name, port) = if let Some(rest) = raw.strip_prefix('[') {
        let (inside, after) = rest.split_once(']')?;
        inside.parse::<Ipv6Addr>().ok()?;
        let port = match after {
            "" => None,
            _ => Some(after.strip_prefix(':')?),
        };
        (format!("[{inside}]"), port)
    } else {
        let (name, port) = match raw.split_once(':') {
            Some((name, port)) => (name, Some(port)),
            None => (raw.as_str(), None),
        };
        if name.parse::<Ipv4Addr>().is_err() && !is_host_name(name) {
            return None;
        }
        (name.to_owned(), port)
    };
    if let Some(port) = port {
        let ok = (1..=5).contains(&port.len())
            && port.bytes().all(|b| b.is_ascii_digit())
            && port.parse::<u16>().is_ok_and(|p| p > 0);
        if !ok {
            return None;
        }
    }
    let host = match port {
        Some(port) => format!("{name}:{port}"),
        None => name.clone(),
    };
    Some((host, name))
}

/// Letters, digits and inner hyphens, in labels of 1 to 63 joined by dots.
/// A last label of digits only would be read as an address.
fn is_host_name(name: &str) -> bool {
    let labels: Vec<&str> = name.split('.').collect();
    let label_ok = |label: &&str| {
        (1..=63).contains(&label.len())
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    };
    name.len() <= 253
        && labels.iter().all(label_ok)
        && labels
            .last()
            .is_some_and(|last| !last.bytes().all(|b| b.is_ascii_digit()))
}

/// Whether a host (as [`valid_host`] gives its name) is on this machine or
/// a private network, where a server answers plain `http`.
fn is_local(name: &str) -> bool {
    let v4 = |ip: Ipv4Addr| {
        let [a, b, ..] = ip.octets();
        ip.is_loopback()
            || ip.is_private()
            || ip.is_link_local()
            || ip.is_unspecified()
            // Shared address space, as Tailscale uses.
            || (a == 100 && (64..128).contains(&b))
    };
    let address = name
        .strip_prefix('[')
        .and_then(|n| n.strip_suffix(']'))
        .unwrap_or(name);
    match address.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => v4(ip),
        Ok(IpAddr::V6(ip)) => {
            let first = ip.segments().first().copied().unwrap_or_default();
            ip.is_loopback()
                || ip.is_unspecified()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || ip.to_ipv4_mapped().is_some_and(v4)
        }
        Err(_) => {
            !name.contains('.')
                || [".localhost", ".local", ".lan", ".home.arpa", ".internal"]
                    .iter()
                    .any(|end| name.ends_with(end))
        }
    }
}

/// What the tags say about a link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    /// The server's name.
    pub site: String,
    pub title: String,
    /// Never empty: WhatsApp shows no preview without one.
    pub description: String,
    /// The page's own absolute address.
    pub url: String,
    pub images: Vec<CardImage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardImage {
    pub url: String,
    pub width: u32,
    pub height: u32,
    pub alt: String,
}

/// The card as `<meta>` tags, every value escaped.
pub fn render(card: &Card) -> String {
    let mut out = String::new();
    let mut meta = |attr: &str, key: &str, value: &str| {
        out.push_str(&format!(
            "<meta {attr}=\"{key}\" content=\"{}\">\n",
            escape_html(value)
        ));
    };
    meta("property", "og:type", "website");
    meta("property", "og:site_name", &card.site);
    meta("property", "og:title", &card.title);
    meta("property", "og:description", &card.description);
    meta("property", "og:url", &card.url);
    for image in &card.images {
        meta("property", "og:image", &image.url);
        meta("property", "og:image:width", &image.width.to_string());
        meta("property", "og:image:height", &image.height.to_string());
        meta("property", "og:image:type", "image/jpeg");
        meta("property", "og:image:alt", &image.alt);
    }
    // Telegram shows the large picture only with this.
    let twitter = if card.images.is_empty() {
        "summary"
    } else {
        "summary_large_image"
    };
    meta("name", "twitter:card", twitter);
    out
}

/// `page` with `extra` near the top of its `<head>`, where WhatsApp, which
/// reads only the first 300 KB, finds it: after its `<meta charset>`, which
/// browsers look for in the first 1024 bytes, else right after `<head>`. A
/// page without a head gets it after `<html>`, or at the start.
pub fn with_head(page: &str, extra: &str) -> String {
    let at = tag_end(page, "<head")
        .map(|head| charset_end(page, head).unwrap_or(head))
        .or_else(|| tag_end(page, "<html"))
        .unwrap_or(0);
    let (before, after) = page.split_at_checked(at).unwrap_or(("", page));
    format!("{before}\n{extra}{after}")
}

/// Where a `<meta charset …>` between `from` and `</head>` ends.
fn charset_end(page: &str, from: usize) -> Option<usize> {
    // Lowercasing ASCII keeps every byte where it was.
    let rest = page.get(from..)?.to_ascii_lowercase();
    let close = rest.find("</head").unwrap_or(rest.len());
    let start = rest.get(..close)?.find("<meta charset")?;
    let end = rest.get(start..)?.find('>')?;
    from.checked_add(start)?.checked_add(end)?.checked_add(1)
}

/// Where the first `<name …>` tag ends, in any case.
fn tag_end(page: &str, name: &str) -> Option<usize> {
    let bytes = page.as_bytes();
    let pattern = name.as_bytes();
    let mut from = 0usize;
    while let Some(found) = bytes.get(from..).and_then(|rest| {
        rest.windows(pattern.len())
            .position(|w| w.eq_ignore_ascii_case(pattern))
    }) {
        let after = from.saturating_add(found).saturating_add(pattern.len());
        match bytes.get(after) {
            Some(b'>') => return after.checked_add(1),
            Some(b) if b.is_ascii_whitespace() || *b == b'/' => {
                let close = bytes.get(after..)?.iter().position(|b| *b == b'>')?;
                return after.checked_add(close)?.checked_add(1);
            }
            // `<header>`, `<htmlfoo>`.
            _ => from = after,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn tags(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn picks_the_language_people_prefer() {
        assert_eq!(language(None), Lang::En);
        assert_eq!(language(Some("")), Lang::En);
        assert_eq!(language(Some("it-IT,it;q=0.9,en;q=0.8")), Lang::It);
        assert_eq!(language(Some("IT")), Lang::It);
        assert_eq!(language(Some("en-US,en;q=0.9,it;q=0.8")), Lang::En);
        // Neither first: the first of the two the app speaks.
        assert_eq!(language(Some("de-DE,de;q=0.9,it;q=0.8,en;q=0.5")), Lang::It);
        assert_eq!(language(Some("fr")), Lang::En);
        // By weight, not by place.
        assert_eq!(language(Some("en;q=0.5, it;q=0.7")), Lang::It);
        assert_eq!(language(Some("it;q=0, en")), Lang::En);
        assert_eq!(language(Some("it;q=0")), Lang::En);
        // Not Italian: a tag that only starts with the letters.
        assert_eq!(language(Some("ita-x, iteration")), Lang::En);
        assert_eq!(language(Some("it;q=2, en;q=0.1")), Lang::En);
        assert_eq!(language(Some("*, it;q=0.1")), Lang::It);
    }

    #[test]
    fn knows_which_paths_are_links() {
        let token = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            link_of(&format!("/s/{token}")),
            Some(Link::Share(token.into()))
        );
        assert_eq!(link_of("/s/0123456789ABCDEF0123456789abcdef"), None);
        assert_eq!(link_of(&format!("/s/{token}/x")), None);
        assert_eq!(link_of("/s/"), None);
        assert_eq!(link_of("/post/12"), Some(Link::Post(12)));
        for not in [
            "/post/",
            "/post/0",
            "/post/-1",
            "/post/+3",
            "/post/12/edit",
            "/post/1e3",
            "/post/99999999999999999999",
            "/posts/12",
            "/",
            "/settings",
        ] {
            assert_eq!(link_of(not), None, "{not}");
        }
    }

    #[test]
    fn counts_in_both_languages() {
        let c = |images, animations, videos| Counts {
            images,
            animations,
            videos,
        };
        let en = |n| count_text(Lang::En, n);
        let it = |n| count_text(Lang::It, n);
        assert_eq!(en(c(0, 0, 0)), None);
        assert_eq!(en(c(1, 0, 0)).as_deref(), Some("1 picture"));
        assert_eq!(en(c(3, 0, 1)).as_deref(), Some("3 pictures and 1 video"));
        assert_eq!(en(c(0, 2, 0)).as_deref(), Some("2 animations"));
        assert_eq!(
            en(c(2, 1, 3)).as_deref(),
            Some("2 pictures, 1 animation and 3 videos")
        );
        assert_eq!(it(c(1, 0, 0)).as_deref(), Some("1 immagine"));
        assert_eq!(it(c(3, 0, 1)).as_deref(), Some("3 immagini e 1 video"));
        assert_eq!(it(c(0, 0, 2)).as_deref(), Some("2 video"));
        assert_eq!(it(c(2, 1, 0)).as_deref(), Some("2 immagini e 1 animazione"));
        assert_eq!(
            it(c(1, 2, 1)).as_deref(),
            Some("1 immagine, 2 animazioni e 1 video")
        );
        let mut counts = Counts::default();
        for kind in [MediaKind::Video, MediaKind::Image, MediaKind::Image] {
            counts.add(kind);
        }
        assert_eq!(counts, c(2, 0, 1));
    }

    #[test]
    fn a_post_is_titled_by_who_and_what_it_shows() {
        let none: Vec<String> = Vec::new();
        let chars = tags(&["hatsune_miku", "kagamine_rin", "megurine_luka", "kaito"]);
        let copy = tags(&["vocaloid"]);
        let art = tags(&["some_artist"]);
        assert_eq!(
            post_title(&chars, &copy, &art, 7),
            "hatsune miku, kagamine rin, megurine luka"
        );
        assert_eq!(post_title(&none, &copy, &art, 7), "vocaloid");
        assert_eq!(post_title(&none, &none, &art, 7), "some artist");
        assert_eq!(post_title(&none, &none, &none, 7), "#7");
    }

    #[test]
    fn a_description_stops_at_a_tag() {
        assert_eq!(post_description(&[], "StarBooru"), "StarBooru");
        assert_eq!(
            post_description(&tags(&["blue_sky", "cloud"]), "StarBooru"),
            "blue sky, cloud"
        );
        // Ten-letter tags: 200 characters hold 16 of them with their commas.
        let many: Vec<String> = (0..40).map(|i| format!("tag_{i:06}")).collect();
        let out = post_description(&many, "x");
        assert!(out.ends_with("tag 000015…"), "{out}");
        assert!(out.chars().count() <= DESCRIPTION_CHARS + 1);
        // Exactly 200 fits whole; one more tag doesn't.
        let fits = vec!["a".repeat(99), "b".repeat(99), "c".into()];
        let whole = post_description(&fits[..2], "x");
        assert_eq!(whole.chars().count(), DESCRIPTION_CHARS);
        assert!(!whole.ends_with('…'));
        assert_eq!(post_description(&fits, "x"), format!("{whole}…"));
        let long = "x".repeat(300);
        let out = post_description(&[long], "x");
        assert_eq!(out.chars().count(), DESCRIPTION_CHARS + 1);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn escapes_every_special_character() {
        assert_eq!(
            escape_html(r#"Tom & "Jerry" <b>'s</b>"#),
            "Tom &amp; &quot;Jerry&quot; &lt;b&gt;&#39;s&lt;/b&gt;"
        );
        let card = Card {
            site: "A&B".into(),
            title: "\"><script>alert(1)</script>".into(),
            description: "it's".into(),
            url: "https://x.example/s/a?b=1&c=2".into(),
            images: Vec::new(),
        };
        let html = render(&card);
        assert!(!html.contains("<script>"));
        assert!(html.contains("content=\"&quot;&gt;&lt;script&gt;"));
        assert!(html.contains("content=\"it&#39;s\""));
        assert!(html.contains("content=\"https://x.example/s/a?b=1&amp;c=2\""));
        assert!(html.contains("<meta name=\"twitter:card\" content=\"summary\">"));
    }

    #[test]
    fn renders_each_image_with_its_size() {
        let image = |n: u32| CardImage {
            url: format!("https://x.example/preview/s/t/{n}.jpg?v=abcd0123"),
            width: 1200,
            height: 800 + n,
            alt: "Trip".into(),
        };
        let card = Card {
            site: "StarBooru".into(),
            title: "Trip".into(),
            description: "2 pictures".into(),
            url: "https://x.example/s/t".into(),
            images: vec![image(0), image(1)],
        };
        let html = render(&card);
        let first = html.find("0.jpg").unwrap();
        let second = html.find("1.jpg").unwrap();
        assert!(first < second);
        assert_eq!(html.matches("og:image:width").count(), 2);
        assert!(html.contains("<meta property=\"og:image:height\" content=\"801\">"));
        assert!(html.contains("<meta property=\"og:image:type\" content=\"image/jpeg\">"));
        assert!(html.contains("content=\"summary_large_image\""));
        assert!(html.starts_with("<meta property=\"og:type\" content=\"website\">"));
    }

    #[test]
    fn tags_go_right_after_the_head() {
        let page = "<!DOCTYPE html>\n<html>\n  <head>\n    <meta charset=\"UTF-8\"><title>S</title></head><body><header></header></body></html>";
        let out = with_head(page, "<meta x>\n");
        // After the charset, which has to stay in the first 1024 bytes.
        assert!(out.starts_with(
            "<!DOCTYPE html>\n<html>\n  <head>\n    <meta charset=\"UTF-8\">\n<meta x>\n<title>"
        ));
        assert_eq!(
            with_head("<HTML><Head lang=\"en\"><title>", "X"),
            "<HTML><Head lang=\"en\">\nX<title>"
        );
        assert_eq!(
            with_head("<head><META Charset=utf-8 /><title>", "X"),
            "<head><META Charset=utf-8 />\nX<title>"
        );
        // A charset in the body isn't the head's.
        assert_eq!(
            with_head("<head></head><body><meta charset=x>", "X"),
            "<head>\nX</head><body><meta charset=x>"
        );
        // `<header>` isn't the head; without one the tags follow `<html>`.
        assert_eq!(
            with_head("<html lang=\"it\"><body><header>", "X"),
            "<html lang=\"it\">\nX<body><header>"
        );
        assert_eq!(with_head("plain", "X"), "\nXplain");
    }

    #[test]
    fn previews_come_from_the_best_picture_at_hand() {
        use MediaKind::*;
        assert_eq!(
            source(Image, "png", Some(true), true, false),
            Some(Source::Sample)
        );
        assert_eq!(
            source(Image, "jpg", Some(false), true, false),
            Some(Source::Original)
        );
        assert_eq!(
            source(Image, "webp", None, false, false),
            Some(Source::Original)
        );
        assert_eq!(
            source(Animation, "gif", Some(false), true, false),
            Some(Source::Original)
        );
        assert_eq!(
            source(Image, "avif", Some(false), true, true),
            Some(Source::Frame)
        );
        assert_eq!(
            source(Image, "jxl", Some(false), true, false),
            Some(Source::Thumb)
        );
        assert_eq!(source(Image, "avif", Some(false), false, false), None);
        assert_eq!(
            source(Video, "mp4", Some(false), true, true),
            Some(Source::Thumb)
        );
        assert_eq!(source(Video, "webm", Some(false), false, true), None);
    }

    #[test]
    fn sizes_are_known_before_anything_is_decoded() {
        assert_eq!(fit(1000, 800, 1200), (1000, 800));
        assert_eq!(fit(4000, 3000, 1200), (1200, 900));
        assert_eq!(fit(1000, 4000, 1200), (300, 1200));
        assert_eq!(fit(1201, 1, 1200), (1200, 1));
        // A big picture's sample is 2560 across, then the preview 1200.
        assert_eq!(
            size(Some(5000), Some(3333), Source::Sample),
            Some((1200, 800))
        );
        assert_eq!(
            size(Some(800), Some(600), Source::Original),
            Some((800, 600))
        );
        assert_eq!(
            size(Some(3000), Some(1000), Source::Original),
            Some((1200, 400))
        );
        // A video's thumbnail is 512 on its long side, and stays so.
        assert_eq!(
            size(Some(1920), Some(1080), Source::Thumb),
            Some((512, 288))
        );
        // A long picture's thumbnail is wider than 512 would leave it.
        assert_eq!(
            size(Some(600), Some(6000), Source::Thumb),
            Some((120, 1200))
        );
        assert_eq!(
            size(Some(4000), Some(3000), Source::Frame),
            Some((1024, 768))
        );
        assert_eq!(size(None, Some(10), Source::Original), None);
        assert_eq!(size(Some(0), Some(10), Source::Original), None);
        assert_eq!(size(Some(-5), Some(10), Source::Original), None);
        // As `image` makes them.
        for (w, h) in [(4000u32, 3000u32), (1999, 1333), (777, 2401), (3001, 7)] {
            let img = image::DynamicImage::new_rgb8(w, h);
            let made = img.resize(
                PREVIEW_SIZE,
                PREVIEW_SIZE,
                image::imageops::FilterType::Nearest,
            );
            assert_eq!(
                fit(w, h, PREVIEW_SIZE),
                (made.width(), made.height()),
                "{w}x{h}"
            );
        }
        assert_eq!(version(&[0xab, 0xcd, 0x01, 0x23, 0xff]), "abcd0123");
    }

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, HeaderValue::from_static(v));
        }
        h
    }

    #[test]
    fn the_address_comes_from_the_setting_the_proxy_or_the_host() {
        let public = Url::parse("https://booru.example.com/sub/").unwrap();
        let none = HeaderMap::new();
        assert_eq!(
            base_url(
                Some(&public),
                true,
                &headers(&[("host", "evil.example")]),
                None
            )
            .as_deref(),
            Some("https://booru.example.com/sub/")
        );
        let base = |trust, pairs: &[(&'static str, &'static str)]| {
            base_url(None, trust, &headers(pairs), None)
        };
        assert_eq!(
            base(false, &[("host", "Booru.Example.com")]).as_deref(),
            Some("https://booru.example.com/")
        );
        assert_eq!(
            base(false, &[("host", "127.0.0.1:8120")]).as_deref(),
            Some("http://127.0.0.1:8120/")
        );
        for local in [
            "localhost:8080",
            "nas",
            "nas.lan",
            "box.local",
            "192.168.1.5",
            "10.1.2.3:80",
            "172.20.0.4",
            "100.101.1.2",
            "[::1]:8080",
            "[fd00::5]",
            "[fe80::1]",
        ] {
            let mut h = HeaderMap::new();
            h.insert("host", HeaderValue::from_str(local).unwrap());
            let out = base_url(None, false, &h, None).unwrap();
            assert!(out.starts_with("http://"), "{local}: {out}");
        }
        for remote in ["8.8.8.8", "[2001:db8::1]:443", "172.32.0.1", "example.org"] {
            let mut h = HeaderMap::new();
            h.insert("host", HeaderValue::from_str(remote).unwrap());
            let out = base_url(None, false, &h, None).unwrap();
            assert!(out.starts_with("https://"), "{remote}: {out}");
        }
        // Behind a trusted proxy its headers come first; otherwise they are ignored.
        let proxied = [
            ("host", "starbooru:8080"),
            ("x-forwarded-host", "booru.example.com"),
            ("x-forwarded-proto", "https"),
        ];
        assert_eq!(
            base(true, &proxied).as_deref(),
            Some("https://booru.example.com/")
        );
        assert_eq!(
            base(false, &proxied).as_deref(),
            Some("http://starbooru:8080/")
        );
        assert_eq!(
            base(
                true,
                &[("host", "booru.example.com"), ("x-forwarded-proto", "http")]
            )
            .as_deref(),
            Some("http://booru.example.com/")
        );
        // The nearest proxy's entry, and an odd scheme is left out.
        assert_eq!(
            base(
                true,
                &[
                    ("x-forwarded-host", "forged.example, booru.example.com"),
                    ("x-forwarded-proto", "gopher")
                ]
            )
            .as_deref(),
            Some("https://booru.example.com/")
        );
        // HTTP/2 has no Host, only the authority.
        assert_eq!(
            base_url(None, false, &none, Some("booru.example.com")).as_deref(),
            Some("https://booru.example.com/")
        );
        assert_eq!(base_url(None, false, &none, None), None);
    }

    #[test]
    fn hosts_are_checked_strictly() {
        for bad in [
            "",
            "booru.example.com/evil",
            "booru.example.com\"><script>",
            "user@booru.example.com",
            "booru.example.com:",
            "booru.example.com:0",
            "booru.example.com:65536",
            "booru.example.com:80:80",
            "booru.example.com:8o",
            "::1",
            "[::1",
            "[nothex]",
            "[::1]x",
            "-booru.example.com",
            "booru-.example.com",
            "booru..example.com",
            "booru.example.com.",
            "booru_x.example.com",
            "1.2.3",
            "999.1.1.1",
            "bóoru.example.com",
            "a b",
        ] {
            assert_eq!(valid_host(bad), None, "{bad}");
        }
        assert_eq!(
            valid_host("Booru.Example.COM:8443"),
            Some(("booru.example.com:8443".into(), "booru.example.com".into()))
        );
        assert_eq!(
            valid_host("[2001:DB8::1]:443"),
            Some(("[2001:db8::1]:443".into(), "[2001:db8::1]".into()))
        );
        assert_eq!(
            valid_host("192.0.2.1"),
            Some(("192.0.2.1".into(), "192.0.2.1".into()))
        );
        assert!(valid_host(&format!("{}.com", "a".repeat(63))).is_some());
        assert!(valid_host(&format!("{}.com", "a".repeat(64))).is_none());
    }
}
