//! Updates: release versions and how they compare, and what administrators
//! see of new releases and of updates (`/admin/updates`).

use std::cmp::Ordering;
use std::fmt;

use chrono::{DateTime, FixedOffset, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The hour automatic updates start at unless the administrator picks
/// another, in the server's time.
pub const DEFAULT_AUTO_UPDATE_HOUR: u8 = 4;

/// A release version, `major.minor.patch` with an optional pre-release
/// (`0.1.0-beta.6`), ordered as Semantic Versioning orders them. Build
/// metadata (`+…`) is ignored.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pre: Vec<Ident>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Ident {
    Number(u64),
    Text(String),
}

impl Version {
    /// `0.2.0`, `v0.2.0` or `0.1.0-beta.6`. Anything else, such as a
    /// branch name in a development build, is `None`.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let text = text.split_once('+').map_or(text, |(v, _)| v);
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (text, None),
        };
        let mut parts = core.split('.').map(number);
        let (Some(Some(major)), Some(Some(minor)), Some(Some(patch)), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return None;
        };
        let pre = match pre {
            None => Vec::new(),
            Some(pre) => pre
                .split('.')
                .map(|part| {
                    if part.is_empty()
                        || !part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    {
                        None
                    } else if part.bytes().all(|b| b.is_ascii_digit()) {
                        part.parse().ok().map(Ident::Number)
                    } else {
                        Some(Ident::Text(part.to_owned()))
                    }
                })
                .collect::<Option<Vec<_>>>()?,
        };
        Some(Self {
            major,
            minor,
            patch,
            pre,
        })
    }

    /// A pre-release, such as `0.1.0-beta.6`.
    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }
}

fn number(part: &str) -> Option<u64> {
    if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    part.parse().ok()
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                // A release comes after its own pre-releases.
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Ident {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Ident::Number(a), Ident::Number(b)) => a.cmp(b),
            (Ident::Number(_), Ident::Text(_)) => Ordering::Less,
            (Ident::Text(_), Ident::Number(_)) => Ordering::Greater,
            (Ident::Text(a), Ident::Text(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for Ident {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        for (i, part) in self.pre.iter().enumerate() {
            f.write_str(if i == 0 { "-" } else { "." })?;
            match part {
                Ident::Number(n) => write!(f, "{n}")?,
                Ident::Text(t) => f.write_str(t)?,
            }
        }
        Ok(())
    }
}

/// Whether `candidate` is a later release than `current`. A version that
/// can't be read, like a branch name, is neither older nor newer.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (Version::parse(candidate), Version::parse(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

/// `GET /admin/updates`: the version running, the newest release, how this
/// install updates and the update under way or the last one's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateStatus {
    /// The version running, as `/info` says it.
    pub current: String,
    /// The newest published release, from the last check that worked.
    /// `None` before the first check, or when there is no release yet.
    #[serde(default)]
    pub latest: Option<Release>,
    /// `latest` is newer than `current`.
    pub available: bool,
    /// When the server last asked, whether that worked or not.
    #[serde(default)]
    pub checked_at: Option<DateTime<Utc>>,
    /// Why the last check failed; `None` when it worked. `latest` then
    /// still holds what the check before it found.
    #[serde(default)]
    pub check_error: Option<String>,
    pub settings: UpdateSettings,
    pub method: UpdateMethod,
    /// The server's clock and time zone, which `settings.auto_hour` is in.
    pub server_time: DateTime<FixedOffset>,
    /// The update under way, or the last one's result.
    #[serde(default)]
    pub run: Option<UpdateRun>,
}

/// A published release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    /// Without the tag's `v`: `0.2.0`.
    pub version: String,
    /// The release's title.
    pub name: String,
    #[serde(default)]
    pub published_at: Option<DateTime<Utc>>,
    /// The release notes, in Markdown.
    pub notes: String,
    /// The release's page.
    pub url: String,
}

/// `PUT /admin/updates/settings`. Each value starts from its environment
/// variable until an admin changes it here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateSettings {
    /// Ask for new releases at start and every 6 hours
    /// (`STARBOORU_UPDATE_CHECK`). Off sends no request at all.
    pub check: bool,
    /// Install new releases by themselves, with a backup first
    /// (`STARBOORU_AUTO_UPDATE`). Only where the server can update itself.
    pub auto: bool,
    /// The hour automatic updates start at, 0 to 23, in the server's time
    /// (`STARBOORU_AUTO_UPDATE_HOUR`).
    pub auto_hour: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum UpdateSettingsError {
    #[error("the hour for automatic updates is 0 to 23")]
    BadHour,
}

impl UpdateSettings {
    pub fn check(&self) -> Result<(), UpdateSettingsError> {
        if self.auto_hour > 23 {
            return Err(UpdateSettingsError::BadHour);
        }
        Ok(())
    }
}

/// How the server runs, which says how it updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateKind {
    /// A Docker container.
    Docker,
    /// A task of a Docker Swarm service.
    Swarm,
    /// The server package: a plain binary under a service manager.
    Package,
    /// Anything else, such as a development build.
    Manual,
}

/// Why the server can't install a release by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateBlock {
    /// A container without the Docker socket, which is the default.
    NoDockerSocket,
    /// The Docker socket is there, but the Engine API failed or didn't know
    /// this container.
    DockerUnreachable,
    /// The image's tag is neither `latest` nor a version, such as a local build.
    UnknownImage,
    /// The container is removed when it stops (`docker run --rm`), so it
    /// couldn't come back if the new one failed.
    RemovedOnStop,
    /// Built from source without a release's version.
    DevelopmentBuild,
    /// No service manager would start the server again.
    NotSupervised,
    /// The server can't write to its own folder.
    ReadOnlyFolder,
    /// No server package for this system.
    Unsupported,
}

/// How this install updates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateMethod {
    pub kind: UpdateKind,
    /// The server can install a release by itself.
    pub can_update: bool,
    /// Why it can't, when it can't.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<UpdateBlock>,
    /// The same in English, for the log or an app without its own words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The image the container or service runs, such as
    /// `ghcr.io/starbooru/starbooru-server:latest`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// The image is a version (`…:0.2.0`), not `latest`. An update pulls the
    /// new version's tag, but the compose file still names the old one, so a
    /// redeploy goes back to it until `STARBOORU_VERSION` is changed.
    #[serde(default)]
    pub pinned: bool,
}

/// Where an update has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateState {
    /// Making sure the release can be installed here.
    Checking,
    /// Downloading the new image or package.
    Downloading,
    /// Backing the database up.
    BackingUp,
    /// Switching to the new version. The server goes away for a moment.
    Restarting,
    /// The new version is running.
    Done,
    /// The update stopped; `error` says why. The version that was running
    /// before still is.
    Failed,
}

impl UpdateState {
    /// Still under way.
    pub fn is_running(self) -> bool {
        !matches!(self, UpdateState::Done | UpdateState::Failed)
    }
}

/// An update, under way or finished. Kept across the restart it causes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateRun {
    pub state: UpdateState,
    /// The version that was running when it started.
    pub from: String,
    /// The version being installed.
    pub to: String,
    /// Started by the automatic updates rather than an administrator.
    #[serde(default)]
    pub automatic: bool,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    /// Why it failed.
    #[serde(default)]
    pub error: Option<String>,
    /// The backup made before it (`GET /admin/backups` lists it).
    #[serde(default)]
    pub backup: Option<String>,
}

/// `POST /admin/updates/install`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateInstall {
    /// Back the database up first. On unless turned off.
    #[serde(default = "yes")]
    pub backup: bool,
}

impl Default for UpdateInstall {
    fn default() -> Self {
        Self { backup: true }
    }
}

fn yes() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn versions_order_as_semver_says() {
        let ordered = [
            "0.1.0-alpha",
            "0.1.0-alpha.1",
            "0.1.0-alpha.beta",
            "0.1.0-beta",
            "0.1.0-beta.2",
            "0.1.0-beta.6",
            "0.1.0-beta.10",
            "0.1.0-beta.11",
            "0.1.0-rc.1",
            "0.1.0",
            "0.1.1",
            "0.2.0",
            "0.9.0",
            "0.10.0",
            "1.0.0",
        ];
        for pair in ordered.windows(2) {
            assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
            assert!(is_newer(pair[1], pair[0]));
            assert!(!is_newer(pair[0], pair[1]));
        }
        assert_eq!(v("v0.2.0"), v("0.2.0"));
        assert_eq!(v("0.2.0+build.5"), v("0.2.0"));
        assert!(!is_newer("0.2.0", "v0.2.0"));
    }

    #[test]
    fn only_versions_are_read() {
        for bad in [
            "",
            "main",
            "v",
            "0.2",
            "0.2.0.1",
            "0.2.x",
            "0..1",
            "-0.2.0",
            "0.2.0-",
            "0.2.0-beta..1",
            "0.2.0-bêta",
            "latest",
            "18446744073709551616.0.0",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad}");
        }
        // A development build named after its branch is never behind.
        assert!(!is_newer("0.2.0", "main"));
        assert!(!is_newer("main", "0.2.0"));
    }

    #[test]
    fn versions_print_as_they_were_written() {
        for text in ["0.2.0", "0.1.0-beta.6", "1.2.3-rc.1.x-y"] {
            assert_eq!(v(text).to_string(), text);
        }
        assert_eq!(v("v0.3.0+abc").to_string(), "0.3.0");
        assert!(v("0.1.0-beta.6").is_prerelease());
        assert!(!v("0.2.0").is_prerelease());
    }

    #[test]
    fn settings_and_requests() {
        let s = |auto_hour| UpdateSettings {
            check: true,
            auto: true,
            auto_hour,
        };
        assert_eq!(s(0).check(), Ok(()));
        assert_eq!(s(23).check(), Ok(()));
        assert_eq!(s(24).check(), Err(UpdateSettingsError::BadHour));
        let install: UpdateInstall = serde_json::from_value(json!({})).unwrap();
        assert!(install.backup);
        let install: UpdateInstall = serde_json::from_value(json!({ "backup": false })).unwrap();
        assert!(!install.backup);
        assert!(UpdateState::Restarting.is_running());
        assert!(!UpdateState::Failed.is_running());
        assert_eq!(
            serde_json::to_value(UpdateState::BackingUp).unwrap(),
            "backing_up"
        );
        assert_eq!(
            serde_json::to_value(UpdateBlock::NoDockerSocket).unwrap(),
            "no_docker_socket"
        );
    }

    #[test]
    fn a_method_leaves_out_what_it_lacks() {
        let method = UpdateMethod {
            kind: UpdateKind::Package,
            can_update: true,
            reason: None,
            message: None,
            image: None,
            pinned: false,
        };
        assert_eq!(
            serde_json::to_value(&method).unwrap(),
            json!({ "kind": "package", "can_update": true, "pinned": false })
        );
    }
}
