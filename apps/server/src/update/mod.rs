//! New releases and updates.
//!
//! The server asks GitHub for the newest release at start and every 6 hours
//! (`github.rs`), keeps the answer in memory and the last good one in
//! `update_state`, and tells administrators when it is newer than the
//! version running. How it installs one depends on how it runs, found at
//! start: a Docker container with the socket replaces itself through a
//! helper container (`helper.rs`), a Swarm task has its service moved to the
//! new image, and the server package swaps its binary and exits for its
//! service manager to start it again (`package.rs`). Anything else says what
//! the administrator does instead.
//!
//! An update runs one at a time: checking, downloading, backing up,
//! restarting. Its state is in `update_state.run`, so the version that
//! starts next reads it and records the result.

pub mod container;
pub mod docker;
pub mod github;
pub mod helper;
pub mod package;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, TimeDelta, Timelike, Utc};
use serde_json::{Value, json};
use starbooru_core::{
    UpdateBlock, UpdateKind, UpdateMethod, UpdateRun, UpdateState, UpdateStatus, Version,
};
use tokio::sync::{Notify, watch};
use tracing::{info, warn};

use self::docker::Docker;
use self::github::Found;
use crate::backup;
use crate::error::{AppError, AppResult};
use crate::settings;
use crate::state::AppState;

/// Built by the release workflows, which set the version to the tag.
pub const RELEASE_BUILD: bool =
    matches!(option_env!("STARBOORU_VERSION"), Some(v) if !v.is_empty());
/// Between checks.
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
/// A check at start is skipped when one ran this recently, so a server
/// restarting over and over doesn't use up GitHub's limit.
const START_GAP: Duration = Duration::from_secs(10 * 60);
/// An administrator's "check now", at most this often.
const MANUAL_GAP: Duration = Duration::from_secs(60);
const CHECK_TIMEOUT: Duration = Duration::from_secs(20);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// How often automatic updates look at the clock.
const AUTO_TICK: Duration = Duration::from_secs(5 * 60);
/// A version whose automatic update failed is tried again a day later.
const AUTO_RETRY: TimeDelta = TimeDelta::hours(20);
/// The helper has this long to replace the container before the update
/// counts as failed.
const HELPER_WITHIN: Duration = Duration::from_secs(10 * 60);
/// What the package's server exits with to be started again on the new binary.
pub const RESTART_EXIT_CODE: u8 = 75;
const USER_AGENT: &str = concat!("StarBooru/", env!("CARGO_PKG_VERSION"));
/// The image's version label, set by the `Dockerfile`.
const VERSION_LABEL: &str = "org.opencontainers.image.version";

/// What the update checker knows.
#[derive(Debug, Clone, Default)]
struct Known {
    found: Option<Found>,
    checked_at: Option<DateTime<Utc>>,
    error: Option<String>,
}

/// How this server runs, and what that means for updating it.
#[derive(Debug, Clone)]
pub struct Detected {
    pub info: UpdateMethod,
    pub how: How,
}

#[derive(Debug, Clone)]
pub enum How {
    Docker { docker: Docker, id: String },
    Swarm { docker: Docker, service: String },
    Package { exe: PathBuf },
    None,
}

impl Detected {
    fn manual(kind: UpdateKind, reason: UpdateBlock, message: impl Into<String>) -> Self {
        Self {
            info: UpdateMethod {
                kind,
                can_update: false,
                reason: Some(reason),
                message: Some(message.into()),
                image: None,
                pinned: false,
            },
            how: How::None,
        }
    }
}

pub struct Updates {
    http: Option<reqwest::Client>,
    /// For the server package, which takes longer than a check.
    downloads: Option<reqwest::Client>,
    known: Mutex<Known>,
    method: Mutex<Detected>,
    run: Mutex<Option<UpdateRun>>,
    /// One update at a time.
    busy: Arc<tokio::sync::Mutex<()>>,
    /// One check at a time.
    checking: tokio::sync::Mutex<()>,
    manual_check: Mutex<Option<Instant>>,
    /// Wakes the check loop when the settings change.
    wake: Notify,
    /// Set when the server should exit for its new binary.
    restart: watch::Sender<bool>,
}

impl Updates {
    pub fn new() -> Self {
        // sqlx already uses ring; Err means a provider is installed already.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = |timeout| {
            reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(Duration::from_secs(10))
                .timeout(timeout)
                .build()
                .map_err(|e| warn!("update checks are off: cannot start the HTTP client: {e}"))
                .ok()
        };
        Self {
            http: client(CHECK_TIMEOUT),
            downloads: client(DOWNLOAD_TIMEOUT),
            known: Mutex::default(),
            method: Mutex::new(Detected::manual(
                UpdateKind::Manual,
                UpdateBlock::Unsupported,
                "the server is still finding out how it runs",
            )),
            run: Mutex::default(),
            busy: Arc::default(),
            checking: tokio::sync::Mutex::default(),
            manual_check: Mutex::default(),
            wake: Notify::new(),
            restart: watch::Sender::new(false),
        }
    }

    fn known(&self) -> Known {
        self.known
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn method(&self) -> Detected {
        self.method
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn set_method(&self, detected: Detected) {
        *self.method.lock().unwrap_or_else(PoisonError::into_inner) = detected;
    }

    fn run(&self) -> Option<UpdateRun> {
        self.run
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Changes `ready` to true when the server should exit and be started
    /// again on its new binary.
    pub fn restart_requested(&self) -> watch::Receiver<bool> {
        self.restart.subscribe()
    }

    /// Wakes the check loop, so new settings apply at once.
    pub fn settings_changed(&self) {
        self.wake.notify_one();
    }
}

/// Where the helper leaves its result.
fn result_path(state: &AppState) -> PathBuf {
    std::path::absolute(&state.config.data_dir)
        .unwrap_or_else(|_| state.config.data_dir.clone())
        .join("update-result.json")
}

/// At start: reads what earlier runs knew, finishes the record of an update
/// that restarted the server, and finds out how the server runs.
pub async fn start(state: &AppState) {
    if let Err(e) = load(state).await {
        warn!("the update state can't be read: {e}");
    }
    resume(state).await;
    let detected = detect(state).await;
    match &detected.info {
        m if m.can_update => info!("updates: this server can update itself ({:?})", m.kind),
        m => info!(
            "updates: this server doesn't update itself: {}",
            m.message.as_deref().unwrap_or("")
        ),
    }
    state.updates.set_method(detected);
}

/// latest, checked_at, check_error, run.
type StateRow = (
    Option<String>,
    Option<DateTime<Utc>>,
    Option<String>,
    Option<String>,
);

async fn load(state: &AppState) -> AppResult<()> {
    let row: Option<StateRow> =
        sqlx::query_as("SELECT latest::text, checked_at, check_error, run::text FROM update_state")
            .fetch_optional(&state.db)
            .await?;
    let (latest, checked_at, error, run) = row.unwrap_or_default();
    *state
        .updates
        .known
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Known {
        found: latest.and_then(|l| serde_json::from_str(&l).ok()),
        checked_at,
        error,
    };
    *state
        .updates
        .run
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = run.and_then(|r| serde_json::from_str(&r).ok());
    Ok(())
}

/// Records the result of an update the server restarted for.
async fn resume(state: &AppState) {
    let Some(mut run) = state.updates.run().filter(|r| r.state.is_running()) else {
        return;
    };
    let outcome_path = result_path(state);
    let outcome = helper::Outcome::read(&outcome_path).filter(|o| o.to == run.to);
    let _ = std::fs::remove_file(&outcome_path);
    let now_on = Version::parse(starbooru_core::VERSION);
    let restarted = run.state == UpdateState::Restarting;
    if restarted && now_on.is_some() && now_on == Version::parse(&run.to) {
        run.state = UpdateState::Done;
        info!("updated from {} to {}", run.from, run.to);
    } else {
        run.state = UpdateState::Failed;
        run.error = Some(match outcome.and_then(|o| o.error) {
            Some(error) => error,
            None if restarted => format!(
                "the server started again on {} instead of {}",
                starbooru_core::VERSION,
                run.to
            ),
            None => "the server stopped during the update".to_owned(),
        });
        warn!(
            "the update to {} failed: {}",
            run.to,
            run.error.as_deref().unwrap_or("")
        );
    }
    run.finished_at = Some(Utc::now());
    if let Err(e) = save_run(state, &run).await {
        warn!("the update's result can't be saved: {e}");
    }
}

async fn save_run(state: &AppState, run: &UpdateRun) -> AppResult<()> {
    *state
        .updates
        .run
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(run.clone());
    let text = serde_json::to_string(run).map_err(AppError::internal)?;
    sqlx::query("UPDATE update_state SET run = $1::jsonb")
        .bind(text)
        .execute(&state.db)
        .await?;
    Ok(())
}

/// What `GET /admin/updates` answers.
pub async fn status(state: &AppState) -> AppResult<UpdateStatus> {
    let settings = settings::updates(state).await?;
    let known = state.updates.known();
    let latest = known.found.map(|f| f.release);
    Ok(UpdateStatus {
        current: starbooru_core::VERSION.to_owned(),
        available: latest
            .as_ref()
            .is_some_and(|r| starbooru_core::is_newer(&r.version, starbooru_core::VERSION)),
        latest,
        checked_at: known.checked_at,
        check_error: known.error,
        settings,
        method: state.updates.method().info,
        server_time: Local::now().fixed_offset(),
        run: state.updates.run(),
    })
}

/// Asks for the newest release now and keeps the answer.
pub async fn check(state: &AppState) {
    let _one = state.updates.checking.lock().await;
    let answer = match &state.updates.http {
        Some(http) => github::latest(http, &state.config.updates.release_url).await,
        None => Err("the server's HTTP client didn't start".to_owned()),
    };
    let now = Utc::now();
    let saved = match &answer {
        Ok(found) => {
            let text = found.as_ref().and_then(|f| serde_json::to_string(f).ok());
            sqlx::query(
                "UPDATE update_state SET latest = $1::jsonb, checked_at = $2, check_error = NULL",
            )
            .bind(text)
            .bind(now)
            .execute(&state.db)
            .await
        }
        Err(error) => {
            sqlx::query("UPDATE update_state SET checked_at = $1, check_error = $2")
                .bind(now)
                .bind(error)
                .execute(&state.db)
                .await
        }
    };
    if let Err(e) = saved {
        warn!("the update check's answer can't be saved: {e}");
    }
    let mut known = state
        .updates
        .known
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    known.checked_at = Some(now);
    match answer {
        Ok(found) => {
            if let Some(f) = &found
                && starbooru_core::is_newer(&f.release.version, starbooru_core::VERSION)
                && known.found.as_ref() != Some(f)
            {
                info!("StarBooru server {} is out", f.release.version);
            }
            known.found = found;
            known.error = None;
        }
        Err(error) => {
            warn!("checking for a new release failed: {error}");
            known.error = Some(error);
        }
    }
}

/// `POST /admin/updates/check`: refused within a minute of the last one.
pub async fn check_now(state: &AppState) -> AppResult<()> {
    if !settings::updates(state).await?.check {
        return Err(AppError::Denied(
            "checks for new releases are turned off in the update settings",
        ));
    }
    {
        let mut last = state
            .updates
            .manual_check
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(wait) = last.and_then(|at| MANUAL_GAP.checked_sub(at.elapsed())) {
            return Err(AppError::TooManyRequests {
                retry_after: wait.as_secs().max(1),
            });
        }
        *last = Some(Instant::now());
    }
    check(state).await;
    Ok(())
}

/// Checks at start and every 6 hours while checks are on.
pub async fn run_checks(state: AppState) {
    let mut first = true;
    loop {
        let on = match settings::updates(&state).await {
            Ok(s) => s.check,
            Err(e) => {
                warn!("the update settings can't be read: {e}");
                false
            }
        };
        let gap = if first { START_GAP } else { CHECK_EVERY };
        first = false;
        let since = |known: &Known| {
            known
                .checked_at
                .and_then(|at| (Utc::now() - at).to_std().ok())
        };
        if on && since(&state.updates.known()).is_none_or(|ago| ago >= gap) {
            check(&state).await;
        }
        let wait = if on {
            let ago = since(&state.updates.known()).unwrap_or_default();
            CHECK_EVERY.saturating_sub(ago).max(Duration::from_secs(60))
        } else {
            Duration::from_secs(3600)
        };
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = state.updates.wake.notified() => {}
        }
    }
}

/// Installs new releases by themselves at the hour the administrator chose.
pub async fn run_auto(state: AppState) {
    let mut tick = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(60),
        AUTO_TICK,
    );
    loop {
        tick.tick().await;
        match auto_due(&state, Local::now().hour(), Utc::now()).await {
            Ok(true) => match install(&state, true, true).await {
                Ok(run) => info!("automatic update to {} started", run.to),
                Err(e) => warn!("the automatic update didn't start: {e}"),
            },
            Ok(false) => {}
            Err(e) => warn!("automatic updates: {e}"),
        }
    }
}

/// Whether an automatic update should start now.
async fn auto_due(state: &AppState, hour: u32, now: DateTime<Utc>) -> AppResult<bool> {
    let settings = settings::updates(state).await?;
    if !settings.auto || hour != u32::from(settings.auto_hour) {
        return Ok(false);
    }
    let Some(found) = newer(state) else {
        return Ok(false);
    };
    if !state.updates.method().info.can_update {
        return Ok(false);
    }
    let tried_lately = state.updates.run().is_some_and(|run| {
        run.state.is_running()
            || (run.to == found.release.version
                && run.state == UpdateState::Failed
                && run.finished_at.is_some_and(|at| now - at < AUTO_RETRY))
    });
    Ok(!tried_lately)
}

/// The newest release, if it is newer than this one.
fn newer(state: &AppState) -> Option<Found> {
    state
        .updates
        .known()
        .found
        .filter(|f| starbooru_core::is_newer(&f.release.version, starbooru_core::VERSION))
}

/// Starts installing the newest release. Answers once it has started; the
/// run's state says how it goes.
pub async fn install(state: &AppState, automatic: bool, backup: bool) -> AppResult<UpdateRun> {
    let running = || AppError::Conflict {
        code: "update_running",
        message: "an update is under way already".into(),
    };
    let guard = state
        .updates
        .busy
        .clone()
        .try_lock_owned()
        .map_err(|_| running())?;
    if state.updates.run().is_some_and(|r| r.state.is_running()) {
        return Err(running());
    }
    let found = newer(state).ok_or_else(|| AppError::Conflict {
        code: "up_to_date",
        message: "this server runs the newest release".into(),
    })?;
    let method = state.updates.method().info;
    if !method.can_update {
        return Err(AppError::Conflict {
            code: "cannot_update",
            message: method
                .message
                .unwrap_or_else(|| "this server can't update itself".into()),
        });
    }
    if backup && !backup::available(state).await {
        return Err(AppError::Conflict {
            code: "backup_unavailable",
            message: "this server can't make backups (pg_dump is missing or turned off); update without one".into(),
        });
    }
    let run = UpdateRun {
        state: UpdateState::Checking,
        from: starbooru_core::VERSION.to_owned(),
        to: found.release.version.clone(),
        automatic,
        started_at: Utc::now(),
        finished_at: None,
        error: None,
        backup: None,
    };
    save_run(state, &run).await?;
    info!("update to {} started", run.to);
    let task_state = state.clone();
    let mut task_run = run.clone();
    tokio::spawn(async move {
        let _one = guard;
        let state = task_state;
        if let Err(error) = perform(&state, &mut task_run, &found, backup).await {
            warn!("the update to {} failed: {error}", task_run.to);
            task_run.state = UpdateState::Failed;
            task_run.error = Some(error);
            task_run.finished_at = Some(Utc::now());
            if let Err(e) = save_run(&state, &task_run).await {
                warn!("the update's result can't be saved: {e}");
            }
        }
    });
    Ok(run)
}

async fn phase(state: &AppState, run: &mut UpdateRun, next: UpdateState) -> Result<(), String> {
    run.state = next;
    save_run(state, run)
        .await
        .map_err(|e| format!("the update's state can't be saved: {e}"))
}

async fn perform(
    state: &AppState,
    run: &mut UpdateRun,
    found: &Found,
    backup: bool,
) -> Result<(), String> {
    // Found at start; what a container is doesn't change while it runs, and
    // the Docker path reads its own container again below.
    let detected = state.updates.method();
    if !detected.info.can_update {
        return Err(detected
            .info
            .message
            .unwrap_or_else(|| "this server can't update itself".into()));
    }
    match detected.how {
        How::Docker { docker, id } => {
            phase(state, run, UpdateState::Downloading).await?;
            let own = docker.container(&id).await?;
            let image = text(&own, "/Config/Image");
            let target = container::target(
                image,
                state.config.updates.image.as_deref(),
                &found.release.version,
            )?;
            info!("update: downloading {}", target.reference());
            docker
                .pull(&target.repo, &target.tag)
                .await
                .map_err(|e| format!("{} can't be downloaded: {e}", target.reference()))?;
            let pulled = docker.image(&target.reference()).await?;
            check_image(&own, &pulled, &target.reference(), &run.to)?;
            back_up(state, run, backup).await?;
            phase(state, run, UpdateState::Restarting).await?;
            start_helper(state, &docker, &own, &target.reference(), &run.to).await?;
            wait_for_helper(state, run).await
        }
        How::Swarm { docker, service } => {
            back_up(state, run, backup).await?;
            phase(state, run, UpdateState::Restarting).await?;
            let current = docker
                .get(&format!("/services/{}", docker::segment(&service)))
                .await?;
            let image = text(&current, "/Spec/TaskTemplate/ContainerSpec/Image");
            let unpinned = image.split('@').next().unwrap_or(image);
            let target = container::target(
                unpinned,
                state.config.updates.image.as_deref(),
                &found.release.version,
            )?;
            let version = current
                .pointer("/Version/Index")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let spec = current
                .get("Spec")
                .and_then(|s| container::service_spec(s, &target.reference()))
                .ok_or("Swarm's description of the service can't be read")?;
            docker
                .post(
                    &format!(
                        "/services/{}/update?version={version}",
                        docker::segment(&service)
                    ),
                    Some(&spec),
                )
                .await
                .map_err(|e| format!("Swarm didn't take the new image: {e}"))?;
            info!("update: Swarm moves the service to {}", target.reference());
            wait_for_helper(state, run).await
        }
        How::Package { exe } => {
            let Some(http) = &state.updates.downloads else {
                return Err("the server's HTTP client didn't start".into());
            };
            phase(state, run, UpdateState::Downloading).await?;
            let new = package::fetch(http, found, &run.to, &exe).await?;
            let ready = async {
                back_up(state, run, backup).await?;
                phase(state, run, UpdateState::Restarting).await?;
                package::swap(&exe, &new)
                    .map_err(|e| format!("the new binary can't be put in place: {e}"))
            }
            .await;
            if ready.is_err() {
                let _ = tokio::fs::remove_file(&new).await;
            }
            ready?;
            info!("update: {} is in place; restarting", run.to);
            state.updates.restart.send_replace(true);
            Ok(())
        }
        How::None => Err("this server can't update itself".into()),
    }
}

/// The pulled image must be new, and the version it says it is.
fn check_image(own: &Value, pulled: &Value, reference: &str, to: &str) -> Result<(), String> {
    if text(pulled, "/Id") == text(own, "/Image") {
        return Err(format!(
            "{reference} is still the version running; the new one may not be published yet, try again later"
        ));
    }
    let label = text(
        pulled,
        &format!("/Config/Labels/{}", VERSION_LABEL.replace('/', "~1")),
    );
    if !label.is_empty() && Version::parse(label) != Version::parse(to) {
        return Err(format!("{reference} is version {label}, not {to}"));
    }
    Ok(())
}

async fn back_up(state: &AppState, run: &mut UpdateRun, wanted: bool) -> Result<(), String> {
    if !wanted {
        return Ok(());
    }
    phase(state, run, UpdateState::BackingUp).await?;
    let made = backup::create(state, false).await.map_err(|e| match e {
        AppError::Internal(detail) => {
            warn!("the backup before the update failed: {detail}");
            "the backup failed; the server's log says why".to_owned()
        }
        other => format!("the backup failed: {other}"),
    })?;
    run.backup = Some(made.name);
    save_run(state, run)
        .await
        .map_err(|e| format!("the update's state can't be saved: {e}"))
}

/// Starts the helper container that replaces this one.
async fn start_helper(
    state: &AppState,
    docker: &Docker,
    own: &Value,
    image: &str,
    to: &str,
) -> Result<(), String> {
    let result = result_path(state);
    let _ = std::fs::remove_file(&result);
    let mounts: Vec<Value> = own
        .get("Mounts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let (kind, source) = match text(m, "/Type") {
                "bind" => ("bind", text(m, "/Source")),
                "volume" => ("volume", text(m, "/Name")),
                _ => return None,
            };
            Some(json!({
                "Type": kind,
                "Source": source,
                "Target": text(m, "/Destination"),
                "ReadOnly": m.get("RW").and_then(Value::as_bool) == Some(false),
            }))
        })
        .collect();
    let mut groups: Vec<Value> = own
        .pointer("/HostConfig/GroupAdd")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(gid) = socket_group(docker.socket()) {
        groups.push(json!(gid.to_string()));
    }
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/app/starbooru-server".into());
    let mut env = vec![
        format!("{}={}", helper::ENV_CONTAINER, text(own, "/Id")),
        format!("{}={image}", helper::ENV_IMAGE),
        format!("{}={to}", helper::ENV_TO),
        format!("{}={}", helper::ENV_RESULT, result.display()),
    ];
    if let Ok(host) = std::env::var("DOCKER_HOST") {
        env.push(format!("DOCKER_HOST={host}"));
    }
    let body = json!({
        "Image": text(own, "/Image"),
        "Entrypoint": [exe],
        "Cmd": ["update-helper"],
        "User": text(own, "/Config/User"),
        "Env": env,
        "Labels": { "eu.filipnone.starbooru.update-helper": to },
        "Healthcheck": { "Test": ["NONE"] },
        "HostConfig": {
            "Mounts": mounts,
            "GroupAdd": groups,
            "AutoRemove": true,
            "NetworkMode": "none",
        },
    });
    let name = format!(
        "{}-update-{}",
        container::name_of(own),
        Utc::now().format("%Y%m%d%H%M%S")
    );
    let created = docker
        .post(
            &format!("/containers/create?name={}", docker::segment(&name)),
            Some(&body),
        )
        .await
        .map_err(|e| format!("the update helper can't be made: {e}"))?;
    let id = text(&created, "/Id");
    docker
        .post(&format!("/containers/{id}/start"), None)
        .await
        .map_err(|e| format!("the update helper doesn't start: {e}"))?;
    info!("update: {name} replaces this container with {image}");
    Ok(())
}

/// The group that owns the Docker socket, for the helper to reach it too.
fn socket_group(socket: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(socket).ok().map(|m| m.gid())
}

/// The helper (or Swarm) stops this server once the new one is up. If this
/// server is still here after a while, the update didn't happen.
async fn wait_for_helper(state: &AppState, run: &UpdateRun) -> Result<(), String> {
    let result = result_path(state);
    let since = Instant::now();
    while since.elapsed() < HELPER_WITHIN {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if let Some(outcome) = helper::Outcome::read(&result).filter(|o| o.to == run.to && !o.ok) {
            let _ = std::fs::remove_file(&result);
            return Err(outcome
                .error
                .unwrap_or_else(|| "the update helper failed".into()));
        }
    }
    Err("the container wasn't replaced; the update helper's or Swarm's log may say why".into())
}

fn text<'a>(value: &'a Value, pointer: &str) -> &'a str {
    value.pointer(pointer).and_then(Value::as_str).unwrap_or("")
}

/// How this server runs.
pub async fn detect(state: &AppState) -> Detected {
    if in_container() {
        detect_docker(state).await
    } else {
        detect_package()
    }
}

fn in_container() -> bool {
    Path::new("/.dockerenv").exists()
        || Path::new("/run/.containerenv").exists()
        || std::fs::read_to_string("/proc/self/mountinfo")
            .is_ok_and(|m| m.contains("/docker/containers/"))
}

const NO_SOCKET: &str = "this container can't reach Docker: pull the new image and start the container again (`docker compose pull && docker compose up -d`, or Redeploy in Dokploy), or give it the Docker socket (see the deployment guide)";

async fn detect_docker(state: &AppState) -> Detected {
    let unreachable =
        |why: String| Detected::manual(UpdateKind::Docker, UpdateBlock::DockerUnreachable, why);
    let docker = match Docker::find(std::env::var("DOCKER_HOST").ok().as_deref()) {
        Ok(Some(docker)) => docker,
        Ok(None) => {
            return Detected::manual(UpdateKind::Docker, UpdateBlock::NoDockerSocket, NO_SOCKET);
        }
        Err(e) => return unreachable(e),
    };
    let read = |path: &str| std::fs::read_to_string(path).unwrap_or_default();
    let Some(id) = container::own_id(
        &read("/proc/self/mountinfo"),
        &read("/proc/self/cgroup"),
        &read("/etc/hostname"),
    ) else {
        return unreachable("Docker can't tell which container this is".into());
    };
    let own = match docker.container(&id).await {
        Ok(own) => own,
        Err(e) => return unreachable(format!("Docker didn't describe this container: {e}")),
    };
    let image = text(&own, "/Config/Image").to_owned();
    let repo = state.config.updates.image.as_deref();
    if let Some(service) = own
        .pointer(&format!(
            "/Config/Labels/{}",
            container::SWARM_SERVICE.replace('/', "~1")
        ))
        .and_then(Value::as_str)
    {
        let spec = match docker
            .get(&format!("/services/{}", docker::segment(service)))
            .await
        {
            Ok(spec) => spec,
            Err(e) => {
                return Detected::manual(
                    UpdateKind::Swarm,
                    UpdateBlock::DockerUnreachable,
                    format!(
                        "Swarm's service can't be read here (the server must run on a manager): {e}"
                    ),
                );
            }
        };
        let service_image = text(&spec, "/Spec/TaskTemplate/ContainerSpec/Image");
        let unpinned = service_image.split('@').next().unwrap_or(service_image);
        return match container::target(unpinned, repo, "0.0.0") {
            Ok(target) => Detected {
                info: UpdateMethod {
                    kind: UpdateKind::Swarm,
                    can_update: true,
                    reason: None,
                    message: None,
                    image: Some(unpinned.to_owned()),
                    pinned: target.pinned,
                },
                how: How::Swarm {
                    docker,
                    service: service.to_owned(),
                },
            },
            Err(why) => {
                let mut d = Detected::manual(UpdateKind::Swarm, UpdateBlock::UnknownImage, why);
                d.info.image = Some(unpinned.to_owned());
                d
            }
        };
    }
    if own
        .pointer("/HostConfig/AutoRemove")
        .and_then(Value::as_bool)
        == Some(true)
    {
        let mut d = Detected::manual(
            UpdateKind::Docker,
            UpdateBlock::RemovedOnStop,
            "the container is removed when it stops (`docker run --rm`), so it couldn't come back if the new version failed",
        );
        d.info.image = Some(image);
        return d;
    }
    match container::target(&image, repo, "0.0.0") {
        Ok(target) => Detected {
            info: UpdateMethod {
                kind: UpdateKind::Docker,
                can_update: true,
                reason: None,
                message: None,
                image: Some(image),
                pinned: target.pinned,
            },
            how: How::Docker { docker, id },
        },
        Err(why) => {
            let mut d = Detected::manual(UpdateKind::Docker, UpdateBlock::UnknownImage, why);
            d.info.image = Some(image);
            d
        }
    }
}

fn detect_package() -> Detected {
    let exe = std::env::current_exe().ok();
    let folder = exe.as_deref().and_then(Path::parent);
    let verdict = package::judge(
        RELEASE_BUILD,
        std::env::consts::OS,
        folder.is_some_and(package::writable),
        std::env::var_os("INVOCATION_ID").is_some_and(|v| !v.is_empty()),
    );
    match (verdict, exe) {
        (Ok(()), Some(exe)) => Detected {
            info: UpdateMethod {
                kind: UpdateKind::Package,
                can_update: true,
                reason: None,
                message: None,
                image: None,
                pinned: false,
            },
            how: How::Package { exe },
        },
        (Err((reason, message)), _) => {
            let kind = if reason == UpdateBlock::DevelopmentBuild {
                UpdateKind::Manual
            } else {
                UpdateKind::Package
            };
            Detected::manual(kind, reason, message)
        }
        (Ok(()), None) => Detected::manual(
            UpdateKind::Package,
            UpdateBlock::Unsupported,
            "the server can't tell where its binary is",
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::http::{Method, StatusCode};
    use axum::routing::get;
    use serde_json::json;

    use super::*;
    use crate::testing;

    /// Files by name.
    type Files = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

    /// A stand-in for GitHub's releases API and download server: answers
    /// `/latest` with whatever the test sets, and serves the files.
    struct Releases {
        url: String,
        answer: Arc<Mutex<(u16, Value)>>,
        files: Files,
        asked: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Releases {
        async fn start() -> Self {
            let answer = Arc::new(Mutex::new((404, json!({ "message": "Not Found" }))));
            let files: Files = Arc::default();
            let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let (a, f, n) = (answer.clone(), files.clone(), asked.clone());
            let app = Router::new()
                .route(
                    "/latest",
                    get(move || {
                        let a = a.clone();
                        n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        async move {
                            let (status, body) = a.lock().unwrap().clone();
                            (StatusCode::from_u16(status).unwrap(), axum::Json(body))
                        }
                    }),
                )
                .route(
                    "/files/{name}",
                    get(
                        move |axum::extract::Path(name): axum::extract::Path<String>| {
                            let f = f.clone();
                            async move {
                                let files = f.lock().unwrap();
                                match files.iter().find(|(n, _)| *n == name) {
                                    Some((_, body)) => (StatusCode::OK, body.clone()),
                                    None => (StatusCode::NOT_FOUND, Vec::new()),
                                }
                            }
                        },
                    ),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            Self {
                url,
                answer,
                files,
                asked,
            }
        }

        fn latest_url(&self) -> url::Url {
            url::Url::parse(&format!("{}/latest", self.url)).unwrap()
        }

        fn answer(&self, status: u16, body: Value) {
            *self.answer.lock().unwrap() = (status, body);
        }

        /// Publishes release `version` with a server package for this machine.
        fn publish(&self, version: &str, package: Vec<u8>) {
            let name = package::package_name(version, std::env::consts::ARCH);
            let sums = format!(
                "{}  {name}\n",
                hex::encode(<Sha256 as sha2::Digest>::digest(&package))
            );
            *self.files.lock().unwrap() = vec![
                (name.clone(), package),
                ("SHA256SUMS".into(), sums.into_bytes()),
            ];
            self.answer(
                200,
                json!({
                    "tag_name": format!("v{version}"),
                    "name": format!("StarBooru server {version}"),
                    "html_url": format!("https://github.com/StarBooru/starbooru-server/releases/tag/v{version}"),
                    "published_at": "2026-12-01T09:00:00Z",
                    "body": "Updates from the app.",
                    "draft": false,
                    "prerelease": false,
                    "assets": [
                        { "name": name, "size": 1, "browser_download_url": format!("{}/files/{name}", self.url) },
                        { "name": "SHA256SUMS", "size": 1, "browser_download_url": format!("{}/files/SHA256SUMS", self.url) }
                    ]
                }),
            );
        }

        fn asked(&self) -> usize {
            self.asked.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    use sha2::Sha256;

    /// A version after the one under test.
    fn next() -> String {
        let v = Version::parse(starbooru_core::VERSION).unwrap();
        format!("{}.{}.0", v.major, v.minor + 1)
    }

    async fn app_with_releases(releases: &Releases) -> Option<testing::TestApp> {
        let url = releases.latest_url();
        let app = testing::app_with(move |c| {
            c.updates.check = true;
            c.updates.release_url = url;
        })
        .await?;
        start(&app.state).await;
        Some(app)
    }

    #[tokio::test]
    async fn admins_see_and_check_for_releases() {
        let releases = Releases::start().await;
        let Some(app) = app_with_releases(&releases).await else {
            return;
        };
        let admin = app.register("admin").await;
        let user = app.register("someone").await;
        let r = app.get("/api/v1/admin/updates", &user).await;
        assert_eq!(r.status, 403);

        let status = app.get("/api/v1/admin/updates", &admin).await.json();
        assert_eq!(status["current"], starbooru_core::VERSION);
        assert_eq!(status["latest"], Value::Null);
        assert_eq!(status["available"], false);
        assert_eq!(status["checked_at"], Value::Null);
        assert_eq!(
            status["settings"],
            json!({ "check": true, "auto": false, "auto_hour": 4 })
        );
        // Tests aren't release builds.
        assert_eq!(status["method"]["kind"], "manual");
        assert_eq!(status["method"]["can_update"], false);
        assert_eq!(status["method"]["reason"], "development_build");
        assert!(status["server_time"].as_str().unwrap().len() >= 20);
        assert_eq!(status["run"], Value::Null);

        let check = |token: &str| {
            let (app, token) = (&app, token.to_owned());
            async move {
                app.call(
                    Method::POST,
                    "/api/v1/admin/updates/check",
                    Some(&token),
                    None,
                )
                .await
            }
        };
        // Every release a pre-release: GitHub answers 404, which is nothing newer.
        let r = check(&admin).await;
        assert_eq!(r.status, 200, "{:?}", r.json());
        assert_eq!(r.json()["latest"], Value::Null);
        assert_eq!(r.json()["available"], false);
        assert_eq!(r.json()["check_error"], Value::Null);
        assert!(r.json()["checked_at"].is_string());
        assert_eq!(releases.asked(), 1);
        // At most once a minute.
        let r = check(&admin).await;
        assert_eq!(r.status, 429);
        assert!(r.headers.contains_key("retry-after"));
        assert_eq!(releases.asked(), 1);

        // A newer release, found by the next check.
        let version = next();
        releases.publish(&version, b"package".to_vec());
        *app.state.updates.manual_check.lock().unwrap() = None;
        let r = check(&admin).await.json();
        assert_eq!(r["available"], true);
        assert_eq!(r["latest"]["version"], version.as_str());
        assert_eq!(r["latest"]["notes"], "Updates from the app.");
        assert_eq!(r["latest"]["published_at"], "2026-12-01T09:00:00Z");
        assert!(r["latest"]["url"].as_str().unwrap().ends_with(&version));

        // GitHub's limit: the error is shown and the release kept.
        releases.answer(403, json!({ "message": "API rate limit exceeded" }));
        super::check(&app.state).await;
        let r = app.get("/api/v1/admin/updates", &admin).await.json();
        assert!(r["check_error"].as_str().unwrap().contains("limit"));
        assert_eq!(r["latest"]["version"], version.as_str());
        assert_eq!(r["available"], true);

        // A restart keeps what was found.
        let storage = crate::storage::Storage::open(&app.state.config.data_dir)
            .await
            .unwrap();
        let again = AppState::new(app.state.config.clone(), app.db().clone(), storage);
        start(&again).await;
        let r = super::status(&again).await.unwrap();
        assert_eq!(r.latest.unwrap().version, version);
        assert!(r.check_error.unwrap().contains("limit"));

        // Turned off, nothing is asked.
        let asked = releases.asked();
        let r = app
            .call(
                Method::PUT,
                "/api/v1/admin/updates/settings",
                Some(&admin),
                Some(json!({ "check": false, "auto": true, "auto_hour": 23 })),
            )
            .await;
        assert_eq!(r.status, 200);
        assert_eq!(
            r.json()["settings"],
            json!({ "check": false, "auto": true, "auto_hour": 23 })
        );
        *app.state.updates.manual_check.lock().unwrap() = None;
        assert_eq!(check(&admin).await.status, 403);
        assert_eq!(releases.asked(), asked);
        let r = app
            .call(
                Method::PUT,
                "/api/v1/admin/updates/settings",
                Some(&admin),
                Some(json!({ "check": true, "auto": false, "auto_hour": 24 })),
            )
            .await;
        assert_eq!(r.status, 400);

        // API keys look but don't touch.
        let key = app
            .call(
                Method::POST,
                "/api/v1/keys",
                Some(&admin),
                Some(json!({ "name": "monitor" })),
            )
            .await
            .json()["token"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(app.get("/api/v1/admin/updates", &key).await.status, 200);
        assert_eq!(check(&key).await.status, 403);
        for path in [
            "/api/v1/admin/updates/install",
            "/api/v1/admin/updates/check",
        ] {
            let r = app.call(Method::POST, path, Some(&user), None).await;
            assert_eq!(r.status, 403, "{path}");
        }
        app.finish().await;
    }

    #[tokio::test]
    async fn a_package_downloads_checks_and_swaps_itself() {
        let releases = Releases::start().await;
        let Some(app) = app_with_releases(&releases).await else {
            return;
        };
        let admin = app.register("admin").await;
        let install = |body: Option<Value>| {
            let (app, admin) = (&app, admin.clone());
            async move {
                app.call(
                    Method::POST,
                    "/api/v1/admin/updates/install",
                    Some(&admin),
                    body,
                )
                .await
            }
        };
        // Nothing newer yet.
        let r = install(None).await;
        assert_eq!(r.status, 409);
        assert_eq!(r.json()["code"], "up_to_date");

        let version = next();
        releases.publish(&version, package::tests::tarball(&version, &version));
        check(&app.state).await;
        // This build can't replace itself.
        let r = install(Some(json!({ "backup": false }))).await;
        assert_eq!(r.json()["code"], "cannot_update");

        // As a supervised package in a folder of its own.
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("starbooru-server");
        std::fs::write(&exe, b"the running binary").unwrap();
        let method = UpdateMethod {
            kind: UpdateKind::Package,
            can_update: true,
            reason: None,
            message: None,
            image: None,
            pinned: false,
        };
        let as_package = || {
            app.state.updates.set_method(Detected {
                info: method.clone(),
                how: How::Package { exe: exe.clone() },
            });
        };
        as_package();
        // A backup needs pg_dump, which tests leave off.
        let r = install(None).await;
        assert_eq!(r.json()["code"], "backup_unavailable");
        assert_eq!(
            app.get("/api/v1/admin/updates", &admin).await.json()["run"],
            Value::Null
        );

        let mut restart = app.state.updates.restart_requested();
        let r = install(Some(json!({ "backup": false }))).await;
        assert_eq!(r.status, 202, "{:?}", r.json());
        let under_way = r.json()["run"]["state"].as_str().unwrap().to_owned();
        assert!(
            ["checking", "downloading", "restarting"].contains(&under_way.as_str()),
            "{under_way}"
        );
        assert_eq!(r.json()["run"]["from"], starbooru_core::VERSION);
        assert_eq!(r.json()["run"]["to"], version.as_str());
        assert_eq!(r.json()["run"]["automatic"], false);
        let again = install(Some(json!({ "backup": false }))).await;
        assert_eq!(again.json()["code"], "update_running");
        tokio::time::timeout(Duration::from_secs(20), restart.wait_for(|r| *r))
            .await
            .unwrap()
            .unwrap();
        let run = app.get("/api/v1/admin/updates", &admin).await.json()["run"].clone();
        assert_eq!(run["state"], "restarting", "{run}");
        assert!(std::fs::read(&exe).unwrap().starts_with(b"#!/bin/sh"));
        assert_eq!(
            std::fs::read(package::beside(&exe, ".old")).unwrap(),
            b"the running binary"
        );
        assert!(!package::beside(&exe, ".new").exists());
        assert!(!package::beside(&exe, ".download").exists());

        // The server that starts next is still this version (a test can't
        // swap itself), so it records a failure, which a new try may follow.
        start(&app.state).await;
        let run = status(&app.state).await.unwrap().run.unwrap();
        assert_eq!(run.state, UpdateState::Failed);
        assert_eq!(
            run.error.unwrap(),
            format!(
                "the server started again on {} instead of {version}",
                starbooru_core::VERSION
            )
        );
        assert!(run.finished_at.is_some());

        // A package that doesn't match its sum is refused before anything moves.
        let wrong = package::tests::tarball(&version, &version);
        releases.publish(&version, wrong.clone());
        releases.files.lock().unwrap()[0].1 = package::tests::tarball(&version, "0.0.1");
        check(&app.state).await;
        as_package();
        std::fs::write(&exe, b"the running binary").unwrap();
        let r = install(Some(json!({ "backup": false }))).await;
        assert_eq!(r.status, 202);
        let run = wait_until_finished(&app.state).await;
        assert_eq!(run.state, UpdateState::Failed);
        assert!(run.error.unwrap().contains("doesn't match its SHA-256"));
        assert_eq!(std::fs::read(&exe).unwrap(), b"the running binary");
        app.finish().await;
    }

    async fn wait_until_finished(state: &AppState) -> UpdateRun {
        for _ in 0..200 {
            if let Some(run) = state.updates.run().filter(|r| !r.state.is_running()) {
                return run;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the update didn't finish");
    }

    #[tokio::test]
    async fn the_next_start_records_the_result() {
        let Some(app) = testing::app().await else {
            return;
        };
        let run = |to: &str, state| UpdateRun {
            state,
            from: "0.1.0".into(),
            to: to.into(),
            automatic: true,
            started_at: Utc::now(),
            finished_at: None,
            error: None,
            backup: Some("starbooru-20261201-040000.dump".into()),
        };
        // The new version came up.
        save_run(
            &app.state,
            &run(starbooru_core::VERSION, UpdateState::Restarting),
        )
        .await
        .unwrap();
        start(&app.state).await;
        let done = app.state.updates.run().unwrap();
        assert_eq!(done.state, UpdateState::Done);
        assert!(done.finished_at.is_some());
        assert_eq!(
            done.backup.as_deref(),
            Some("starbooru-20261201-040000.dump")
        );

        // The helper put the old one back and said why.
        save_run(&app.state, &run("9.0.0", UpdateState::Restarting))
            .await
            .unwrap();
        std::fs::create_dir_all(&app.state.config.data_dir).unwrap();
        let outcome = helper::Outcome {
            to: "9.0.0".into(),
            ok: false,
            error: Some(
                "the new container stopped (exit code 1); the old version is running again".into(),
            ),
        };
        std::fs::write(
            result_path(&app.state),
            serde_json::to_vec(&outcome).unwrap(),
        )
        .unwrap();
        start(&app.state).await;
        let failed = app.state.updates.run().unwrap();
        assert_eq!(failed.state, UpdateState::Failed);
        assert_eq!(failed.error, outcome.error);
        assert!(!result_path(&app.state).exists());

        // The server stopped halfway, before anything changed.
        save_run(&app.state, &run("9.0.0", UpdateState::Downloading))
            .await
            .unwrap();
        start(&app.state).await;
        assert_eq!(
            app.state.updates.run().unwrap().error.as_deref(),
            Some("the server stopped during the update")
        );
        app.finish().await;
    }

    #[tokio::test]
    async fn automatic_updates_wait_for_their_hour() {
        let releases = Releases::start().await;
        let Some(app) = app_with_releases(&releases).await else {
            return;
        };
        let now = Utc::now();
        // Off by default.
        assert!(!auto_due(&app.state, 4, now).await.unwrap());
        crate::settings::save_updates(
            &app.state,
            starbooru_core::UpdateSettings {
                check: true,
                auto: true,
                auto_hour: 4,
            },
        )
        .await
        .unwrap();
        // Nothing newer.
        assert!(!auto_due(&app.state, 4, now).await.unwrap());
        let version = next();
        releases.publish(&version, b"x".to_vec());
        check(&app.state).await;
        // A development build can't.
        assert!(!auto_due(&app.state, 4, now).await.unwrap());
        app.state.updates.set_method(Detected {
            info: UpdateMethod {
                kind: UpdateKind::Package,
                can_update: true,
                reason: None,
                message: None,
                image: None,
                pinned: false,
            },
            how: How::None,
        });
        assert!(auto_due(&app.state, 4, now).await.unwrap());
        assert!(!auto_due(&app.state, 5, now).await.unwrap());
        // A failed try waits a day.
        let failed = UpdateRun {
            state: UpdateState::Failed,
            from: starbooru_core::VERSION.into(),
            to: version.clone(),
            automatic: true,
            started_at: now,
            finished_at: Some(now),
            error: Some("x".into()),
            backup: None,
        };
        save_run(&app.state, &failed).await.unwrap();
        assert!(!auto_due(&app.state, 4, now).await.unwrap());
        assert!(
            auto_due(&app.state, 4, now + TimeDelta::hours(23))
                .await
                .unwrap()
        );
        // One that worked doesn't hold the next back, say after going back by hand.
        let done = UpdateRun {
            state: UpdateState::Done,
            error: None,
            ..failed.clone()
        };
        save_run(&app.state, &done).await.unwrap();
        assert!(auto_due(&app.state, 4, now).await.unwrap());
        let running = UpdateRun {
            state: UpdateState::Downloading,
            to: "9.9.9".into(),
            finished_at: None,
            ..failed
        };
        save_run(&app.state, &running).await.unwrap();
        assert!(!auto_due(&app.state, 4, now).await.unwrap());
        app.finish().await;
    }

    /// Asks GitHub itself. Ignored, as tests don't go online by default:
    /// `cargo test -p starbooru-server github_for_real -- --ignored`.
    #[tokio::test]
    #[ignore = "asks api.github.com"]
    async fn asks_github_for_real() {
        let updates = Updates::new();
        let url = url::Url::parse(crate::config::RELEASES_URL).unwrap();
        let found = github::latest(updates.http.as_ref().unwrap(), &url).await;
        eprintln!("GitHub's newest release: {found:?}");
        match found.unwrap() {
            // Every release so far is a pre-release.
            None => {}
            Some(f) => assert!(!Version::parse(&f.release.version).unwrap().is_prerelease()),
        }
    }
}
