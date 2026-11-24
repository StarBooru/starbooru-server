//! Asking GitHub for the newest release: published, not a draft, not a
//! pre-release. No credentials go with it, so GitHub allows 60 an hour per
//! address; the server asks four times a day.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use starbooru_core::{Release, Version};
use url::Url;

/// A release's notes are a few kilobytes; a bigger answer is no release.
const MAX_ANSWER_BYTES: usize = 1 << 20;

/// A file attached to a release, for the server package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

/// What a check found. Kept in the database as it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Found {
    pub release: Release,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

impl Found {
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    published_at: Option<DateTime<Utc>>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

/// Reads GitHub's answer. `Ok(None)` for a draft or a pre-release, which
/// `releases/latest` doesn't return but another address might.
pub fn parse(body: &[u8]) -> Result<Option<Found>, String> {
    let release: GithubRelease =
        serde_json::from_slice(body).map_err(|_| "GitHub's answer can't be read".to_owned())?;
    if release.draft || release.prerelease {
        return Ok(None);
    }
    let version = Version::parse(&release.tag_name).ok_or_else(|| {
        format!(
            "the newest release's version ({}) can't be read",
            release.tag_name.chars().take(64).collect::<String>()
        )
    })?;
    if version.is_prerelease() {
        return Ok(None);
    }
    let version = version.to_string();
    Ok(Some(Found {
        release: Release {
            name: release
                .name
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| format!("StarBooru server {version}")),
            version,
            published_at: release.published_at,
            notes: release.body.unwrap_or_default(),
            url: release.html_url,
        },
        assets: release
            .assets
            .into_iter()
            .map(|a| Asset {
                name: a.name,
                url: a.browser_download_url,
                size: a.size,
            })
            .collect(),
    }))
}

/// Asks `url` for the newest release. `Ok(None)` when there is none: GitHub
/// answers 404 while every release is a pre-release.
pub async fn latest(client: &reqwest::Client, url: &Url) -> Result<Option<Found>, String> {
    let resp = client
        .get(url.clone())
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .map_err(|_| "GitHub can't be reached: check the server's internet access".to_owned())?;
    match resp.status().as_u16() {
        200 => {}
        404 => return Ok(None),
        403 | 429 => {
            return Err(
                "GitHub's limit on requests from this address was reached; the next check tries again"
                    .to_owned(),
            );
        }
        status => return Err(format!("GitHub answered {status}")),
    }
    let body = read_capped(resp, MAX_ANSWER_BYTES).await?;
    parse(&body)
}

/// The whole body of `resp`, up to `max` bytes.
pub async fn read_capped(mut resp: reqwest::Response, max: usize) -> Result<Vec<u8>, String> {
    if resp
        .content_length()
        .is_some_and(|n| n > u64::try_from(max).unwrap_or(u64::MAX))
    {
        return Err("the answer is too large".to_owned());
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|_| "the answer was cut short".to_owned())?
    {
        if body.len().saturating_add(chunk.len()) > max {
            return Err("the answer is too large".to_owned());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    pub fn answer(tag: &str, prerelease: bool) -> serde_json::Value {
        json!({
            "url": "https://api.github.com/repos/StarBooru/starbooru-server/releases/1",
            "html_url": format!("https://github.com/StarBooru/starbooru-server/releases/tag/{tag}"),
            "tag_name": tag,
            "name": format!("StarBooru server {}", tag.trim_start_matches('v')),
            "draft": false,
            "prerelease": prerelease,
            "published_at": "2026-11-30T10:00:00Z",
            "body": "## What's new\n\n- Updates from the app",
            "assets": [{
                "name": format!("starbooru-server-{}-linux-x86_64.tar.gz", tag.trim_start_matches('v')),
                "size": 7176710,
                "browser_download_url": "https://github.com/StarBooru/starbooru-server/releases/download/x/y.tar.gz",
                "uploader": { "login": "someone" }
            }]
        })
    }

    #[test]
    fn reads_a_release() {
        let body = serde_json::to_vec(&answer("v0.2.0", false)).unwrap();
        let found = parse(&body).unwrap().unwrap();
        assert_eq!(found.release.version, "0.2.0");
        assert_eq!(found.release.name, "StarBooru server 0.2.0");
        assert_eq!(
            found.release.notes,
            "## What's new\n\n- Updates from the app"
        );
        assert_eq!(
            found.release.url,
            "https://github.com/StarBooru/starbooru-server/releases/tag/v0.2.0"
        );
        assert_eq!(
            found.release.published_at.unwrap().to_rfc3339(),
            "2026-11-30T10:00:00+00:00"
        );
        assert_eq!(found.assets.len(), 1);
        assert!(
            found
                .asset("starbooru-server-0.2.0-linux-x86_64.tar.gz")
                .is_some()
        );
    }

    #[test]
    fn pre_releases_and_junk_are_not_releases() {
        let pre = serde_json::to_vec(&answer("v0.1.0-beta.6", true)).unwrap();
        assert_eq!(parse(&pre), Ok(None));
        // Marked a release, but its tag says otherwise.
        let pre = serde_json::to_vec(&answer("v0.3.0-rc.1", false)).unwrap();
        assert_eq!(parse(&pre), Ok(None));
        assert!(parse(b"<html>").is_err());
        let branch = serde_json::to_vec(&answer("nightly", false)).unwrap();
        assert!(parse(&branch).unwrap_err().contains("nightly"));
        // The fields GitHub may leave out.
        let bare = parse(br#"{"tag_name":"v1.0.0"}"#).unwrap().unwrap();
        assert_eq!(bare.release.name, "StarBooru server 1.0.0");
        assert_eq!(bare.release.notes, "");
        assert!(bare.assets.is_empty());
    }
}
