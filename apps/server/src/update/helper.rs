//! `starbooru-server update-helper`: replaces the server's container with a
//! copy on the new image. A process can't replace its own container, so the
//! server starts this in a short-lived container of its own, on the image it
//! runs itself (always there, and known to start), with the Docker socket and
//! the server's volumes.
//!
//! It stops the old container and renames it out of the way, makes the new
//! one under the old name with the same settings, and waits for it to become
//! healthy. Only then does it remove the old one. If the new one fails, it
//! goes, and the old one gets its name back and starts again. Either way the
//! result goes to a file in the data folder, which the server reads when it
//! starts.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::container::{self, Health};
use super::docker::Docker;

pub const ENV_CONTAINER: &str = "STARBOORU_UPDATE_CONTAINER";
pub const ENV_IMAGE: &str = "STARBOORU_UPDATE_TARGET_IMAGE";
pub const ENV_TO: &str = "STARBOORU_UPDATE_TO";
pub const ENV_RESULT: &str = "STARBOORU_UPDATE_RESULT";

/// Time for the server to answer the request that started the update.
const SETTLE: Duration = Duration::from_secs(2);
/// The server shuts down cleanly within this; Docker kills it after.
const STOP_SECS: u32 = 30;
/// The new container has this long to become healthy.
const HEALTHY_WITHIN: Duration = Duration::from_secs(5 * 60);

/// What the helper reports to the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    pub to: String,
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
}

impl Outcome {
    /// The result the helper left at `path`, if any.
    pub fn read(path: &Path) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        if bytes.len() > 64 << 10 {
            return None;
        }
        serde_json::from_slice(&bytes).ok()
    }

    fn write(&self, path: &Path) -> std::io::Result<()> {
        let part = path.with_extension("part");
        std::fs::write(
            &part,
            serde_json::to_vec(self).map_err(std::io::Error::other)?,
        )?;
        std::fs::rename(&part, path)
    }
}

/// The helper's entry point.
pub async fn run() -> ExitCode {
    let var = |name| std::env::var(name).ok().filter(|v: &String| !v.is_empty());
    let (Some(old), Some(image), Some(to), Some(result)) = (
        var(ENV_CONTAINER),
        var(ENV_IMAGE),
        var(ENV_TO),
        var(ENV_RESULT),
    ) else {
        eprintln!("update-helper: the server starts this; it isn't for running by hand");
        return ExitCode::FAILURE;
    };
    let result = PathBuf::from(result);
    let reported = std::cell::RefCell::new(None::<String>);
    let report = |error: &str| {
        // The old server may have read it already; writing it again would leave it behind.
        if reported.borrow().as_deref() == Some(error) {
            return;
        }
        reported.replace(Some(error.to_owned()));
        let failed = Outcome {
            to: to.clone(),
            ok: false,
            error: Some(error.to_owned()),
        };
        if let Err(e) = failed.write(&result) {
            eprintln!("update-helper: can't write {}: {e}", result.display());
        }
    };
    let outcome = match Docker::find(var("DOCKER_HOST").as_deref()) {
        Ok(Some(docker)) => replace(&docker, &old, &image, &report).await,
        Ok(None) => Err("the Docker socket is missing".to_owned()),
        Err(e) => Err(e),
    };
    match outcome {
        Ok(()) => {
            println!("update-helper: the server runs {image} now");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("update-helper: {e}");
            report(&e);
            ExitCode::FAILURE
        }
    }
}

/// Replaces container `old` with a copy on `image`, or leaves it running
/// and says why not. `report` hears of a failure before the old container
/// starts again, as the server reads it when it starts.
async fn replace(
    docker: &Docker,
    old: &str,
    image: &str,
    report: &impl Fn(&str),
) -> Result<(), String> {
    tokio::time::sleep(SETTLE).await;
    let current = docker.container(old).await?;
    let old_id = current
        .get("Id")
        .and_then(Value::as_str)
        .unwrap_or(old)
        .to_owned();
    let name = container::name_of(&current);
    let old_image_id = current.get("Image").and_then(Value::as_str).unwrap_or("");
    let old_image = docker.image(old_image_id).await?;
    let new_image = docker.image(image).await?;
    let new_image_id = new_image.get("Id").and_then(Value::as_str).unwrap_or("");
    let body = container::create_body(&current, &old_image, image, new_image_id);
    let extra = container::extra_networks(&current);

    println!("update-helper: stopping {name}");
    docker
        .post(&format!("/containers/{old_id}/stop?t={STOP_SECS}"), None)
        .await
        .map_err(|e| format!("the old container didn't stop: {e}"))?;
    let parked = format!("{name}-old-{}", chrono::Utc::now().format("%Y%m%d%H%M%S"));
    if let Err(e) = docker
        .post(
            &format!("/containers/{old_id}/rename?{}", query("name", &parked)),
            None,
        )
        .await
    {
        return Err(start_again(
            docker,
            &old_id,
            format!("the old container can't be renamed: {e}"),
            report,
        )
        .await);
    }

    let mut made = None;
    let tried = async {
        let created = docker
            .post(
                &format!("/containers/create?{}", query("name", &name)),
                Some(&body),
            )
            .await
            .map_err(|e| format!("the new container can't be made: {e}"))?;
        let new_id = created
            .get("Id")
            .and_then(Value::as_str)
            .ok_or("Docker didn't name the new container")?
            .to_owned();
        made = Some(new_id.clone());
        for (network, endpoint) in &extra {
            docker
                .post(
                    &format!("/networks/{}/connect", super::docker::segment(network)),
                    Some(&json!({ "Container": new_id, "EndpointConfig": endpoint })),
                )
                .await
                .map_err(|e| format!("the new container can't join {network}: {e}"))?;
        }
        println!("update-helper: starting {name} on {image}");
        start(docker, &new_id)
            .await
            .map_err(|e| format!("the new container doesn't start: {e}"))?;
        healthy(docker, &new_id).await
    }
    .await;

    match tried {
        Ok(()) => {
            // The update is done; an old container left behind is only clutter.
            if let Err(e) = docker
                .delete(&format!("/containers/{old_id}?force=true"))
                .await
            {
                eprintln!("update-helper: the old container {parked} stays: {e}");
            }
            Ok(())
        }
        Err(e) => {
            if let Some(new_id) = &made
                && let Err(gone) = docker
                    .delete(&format!("/containers/{new_id}?force=true"))
                    .await
            {
                eprintln!("update-helper: the new container stays: {gone}");
            }
            match docker
                .post(
                    &format!("/containers/{old_id}/rename?{}", query("name", &name)),
                    None,
                )
                .await
            {
                Ok(_) => Err(start_again(docker, &old_id, e, report).await),
                Err(back) => Err(with_rollback(
                    e,
                    Err(format!("it can't get its name back: {back}")),
                )),
            }
        }
    }
}

/// Starts the old container again after `error`, which is reported first.
/// Returns the error with what became of the old container.
async fn start_again(docker: &Docker, id: &str, error: String, report: &impl Fn(&str)) -> String {
    report(&with_rollback(error.clone(), Ok(())));
    let back = start(docker, id).await;
    with_rollback(error, back)
}

async fn start(docker: &Docker, id: &str) -> Result<(), String> {
    docker
        .post(&format!("/containers/{id}/start"), None)
        .await
        .map(|_| ())
}

/// Waits for the new container to pass its health check.
async fn healthy(docker: &Docker, id: &str) -> Result<(), String> {
    let since = Instant::now();
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let running = i64::try_from(since.elapsed().as_secs()).unwrap_or(i64::MAX);
        let state = docker.container(id).await?;
        match container::health(&state, running) {
            Health::Healthy => return Ok(()),
            Health::Failed(why) => return Err(why),
            Health::Starting if since.elapsed() > HEALTHY_WITHIN => {
                return Err(format!(
                    "the new container wasn't healthy after {} minutes",
                    HEALTHY_WITHIN.as_secs() / 60
                ));
            }
            Health::Starting => {}
        }
    }
}

fn with_rollback(error: String, back: Result<(), String>) -> String {
    match back {
        Ok(()) => format!("{error}; the old version is running again"),
        Err(e) => format!("{error}; the old container couldn't be started again: {e}"),
    }
}

fn query(key: &str, value: &str) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .append_pair(key, value)
        .finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_outcome_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-result.json");
        assert_eq!(Outcome::read(&path), None);
        let failed = Outcome {
            to: "0.4.0".into(),
            ok: false,
            error: Some(
                "the new container stopped (exit code 1); the old version is running again".into(),
            ),
        };
        failed.write(&path).unwrap();
        assert_eq!(Outcome::read(&path), Some(failed));
        assert!(!path.with_extension("part").exists());
        std::fs::write(&path, b"{").unwrap();
        assert_eq!(Outcome::read(&path), None);
    }

    #[test]
    fn says_whether_the_old_one_came_back() {
        assert_eq!(
            with_rollback("it broke".into(), Ok(())),
            "it broke; the old version is running again"
        );
        assert_eq!(
            with_rollback("it broke".into(), Err("no such container".into())),
            "it broke; the old container couldn't be started again: no such container"
        );
    }
}
