//! What an update does to a container, worked out from what Docker says
//! about it: which image to pull, how to make a copy of the container on
//! it, and whether the copy has come up. No I/O, so all of it is tested.

use serde_json::{Map, Value, json};

/// The label Compose keeps the image's id in. It is set to the new image's,
/// so `docker compose up` doesn't make the container again for nothing.
const COMPOSE_IMAGE: &str = "com.docker.compose.image";
/// Swarm's tasks carry their service's id.
pub const SWARM_SERVICE: &str = "com.docker.swarm.service.id";
/// Tags of the image variants for graphics cards.
const VARIANTS: [&str; 2] = ["-vulkan", "-nvidia"];

/// An image reference split into its parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef<'a> {
    pub repo: &'a str,
    pub tag: Option<&'a str>,
    pub digest: Option<&'a str>,
}

/// `repo[:tag][@digest]`. A `:` before the last `/` is a registry's port.
pub fn split_image(image: &str) -> ImageRef<'_> {
    let (name, digest) = match image.split_once('@') {
        Some((name, digest)) => (name, Some(digest)),
        None => (image, None),
    };
    let slash = name.rfind('/').map_or(0, |i| i.saturating_add(1));
    match name.get(slash..).and_then(|last| last.rfind(':')) {
        Some(colon) => {
            let at = slash.saturating_add(colon);
            ImageRef {
                repo: name.get(..at).unwrap_or(name),
                tag: name.get(at.saturating_add(1)..),
                digest,
            }
        }
        None => ImageRef {
            repo: name,
            tag: None,
            digest,
        },
    }
}

/// The image an update pulls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub repo: String,
    pub tag: String,
    /// The container runs a version's tag rather than `latest`.
    pub pinned: bool,
}

impl Target {
    pub fn reference(&self) -> String {
        format!("{}:{}", self.repo, self.tag)
    }
}

/// What to pull to update a container running `image` to `version`:
/// `latest` (and its graphics variants) again, or the new version's tag with
/// the same variant. `repo` replaces the image's repository. `Err` says why
/// there is nothing to pull.
pub fn target(image: &str, repo: Option<&str>, version: &str) -> Result<Target, String> {
    let parts = split_image(image);
    if parts.digest.is_some() {
        return Err(format!(
            "the container runs an image pinned by its digest ({image}); change the compose file to update it"
        ));
    }
    let repo = repo.unwrap_or(parts.repo).to_owned();
    let tag = parts.tag.unwrap_or("latest");
    let (base, variant) = VARIANTS
        .iter()
        .find_map(|v| tag.strip_suffix(v).map(|base| (base, *v)))
        .unwrap_or((tag, ""));
    if base == "latest" {
        return Ok(Target {
            repo,
            tag: tag.to_owned(),
            pinned: false,
        });
    }
    if starbooru_core::Version::parse(base).is_some() {
        return Ok(Target {
            repo,
            tag: format!("{version}{variant}"),
            pinned: true,
        });
    }
    Err(format!(
        "the container runs the image tag `{tag}`, which is neither latest nor a version, such as a local build"
    ))
}

/// The id of the container this process runs in, from what the kernel shows
/// it: Docker's files for the container are mounted from a folder named
/// after it, cgroup v1 names it, and the hostname is its start unless set.
pub fn own_id(mountinfo: &str, cgroup: &str, hostname: &str) -> Option<String> {
    let after_containers = mountinfo.lines().find_map(|line| {
        let mut parts = line.split('/').peekable();
        while let Some(part) = parts.next() {
            if part.ends_with("containers")
                && let Some(next) = parts.peek()
                && is_id(next)
            {
                return Some((*next).to_owned());
            }
        }
        None
    });
    after_containers
        .or_else(|| {
            cgroup
                .split(|c: char| !c.is_ascii_hexdigit())
                .find(|part| is_id(part))
                .map(str::to_owned)
        })
        .or_else(|| {
            let host = hostname.trim();
            (host.len() == 12 && host.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
                .then(|| host.to_owned())
        })
}

fn is_id(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn object(value: Option<&Value>) -> Map<String, Value> {
    value
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn text<'a>(value: &'a Value, pointer: &str) -> &'a str {
    value.pointer(pointer).and_then(Value::as_str).unwrap_or("")
}

/// The container's name without Docker's leading `/`.
pub fn name_of(container: &Value) -> String {
    text(container, "/Name").trim_start_matches('/').to_owned()
}

/// The body of `POST /containers/create` for a copy of `old`, the container
/// as `GET /containers/{id}/json` describes it, on `image`. What the old
/// image (`old_image`, as `GET /images/{id}/json` describes it) set goes, so
/// the new image's own values apply; what the container set itself stays:
/// environment, labels, volumes, ports, restart policy, devices, graphics
/// cards. The primary network is in the body; [`extra_networks`] are joined
/// after. `new_image_id` updates Compose's record of the image.
pub fn create_body(old: &Value, old_image: &Value, image: &str, new_image_id: &str) -> Value {
    let id = text(old, "/Id");
    let image_config = old_image.get("Config").unwrap_or(&Value::Null);
    let mut config = object(old.get("Config"));
    config.insert("Image".into(), json!(image));
    // Docker sets the hostname to the id's start unless it was given one.
    if id
        .get(..12)
        .is_some_and(|short| text(old, "/Config/Hostname") == short)
    {
        config.remove("Hostname");
    }
    for key in [
        "Cmd",
        "Entrypoint",
        "WorkingDir",
        "User",
        "Healthcheck",
        "StopSignal",
        "StopTimeout",
        "Shell",
        "OnBuild",
        "ArgsEscaped",
    ] {
        if config.get(key) == image_config.get(key) {
            config.remove(key);
        }
    }
    // Not a per-container setting any more; the network has it.
    config.remove("MacAddress");
    if let Some(Value::Array(env)) = config.get_mut("Env") {
        let from_image = image_config.get("Env").and_then(Value::as_array);
        env.retain(|e| !from_image.is_some_and(|image_env| image_env.contains(e)));
    }
    for key in ["ExposedPorts", "Volumes"] {
        if let Some(Value::Object(own)) = config.get_mut(key) {
            let from_image = object(image_config.get(key));
            own.retain(|k, _| !from_image.contains_key(k));
        }
    }
    if let Some(Value::Object(labels)) = config.get_mut("Labels") {
        let from_image = object(image_config.get("Labels"));
        labels.retain(|k, v| from_image.get(k) != Some(v));
        if labels.contains_key(COMPOSE_IMAGE) {
            labels.insert(COMPOSE_IMAGE.into(), json!(new_image_id));
        }
    }

    let mut host = object(old.get("HostConfig"));
    // A volume the image declared and nobody named lives only in `Mounts`;
    // without this the copy would get an empty one.
    let mut targets: Vec<String> = host
        .get("Binds")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|bind| bind.split(':').nth(1).map(str::to_owned))
        .collect();
    targets.extend(
        host.get("Mounts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|m| m.get("Target").and_then(Value::as_str).map(str::to_owned)),
    );
    let kept: Vec<Value> = old
        .get("Mounts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|m| text(m, "/Type") == "volume")
        .filter(|m| !targets.iter().any(|t| t == text(m, "/Destination")))
        .filter(|m| !text(m, "/Name").is_empty())
        .map(|m| {
            json!({
                "Type": "volume",
                "Source": text(m, "/Name"),
                "Target": text(m, "/Destination"),
                "ReadOnly": !m.get("RW").and_then(Value::as_bool).unwrap_or(true),
            })
        })
        .collect();
    if !kept.is_empty() {
        let mounts = host.entry("Mounts").or_insert_with(|| json!([]));
        if !mounts.is_array() {
            *mounts = json!([]);
        }
        if let Some(list) = mounts.as_array_mut() {
            list.extend(kept);
        }
    }

    let mut body = config;
    body.insert("HostConfig".into(), Value::Object(host));
    if let Some((name, endpoint)) = primary_network(old) {
        body.insert(
            "NetworkingConfig".into(),
            json!({ "EndpointsConfig": { name: endpoint } }),
        );
    }
    Value::Object(body)
}

/// The network the container is created on, from its network mode.
fn primary_network(old: &Value) -> Option<(String, Value)> {
    let mode = text(old, "/HostConfig/NetworkMode");
    let networks = old.pointer("/NetworkSettings/Networks")?.as_object()?;
    let endpoint = networks.get(mode)?;
    Some((mode.to_owned(), endpoint_config(old, endpoint)))
}

/// The other networks, joined once the copy exists: Docker before API 1.44
/// takes only one at creation.
pub fn extra_networks(old: &Value) -> Vec<(String, Value)> {
    let mode = text(old, "/HostConfig/NetworkMode");
    if matches!(mode, "host" | "none") || mode.starts_with("container:") {
        return Vec::new();
    }
    let primary = if mode == "default" { "bridge" } else { mode };
    old.pointer("/NetworkSettings/Networks")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(name, _)| name.as_str() != primary)
        .map(|(name, endpoint)| (name.clone(), endpoint_config(old, endpoint)))
        .collect()
}

/// What was asked for on a network: aliases, a fixed address, links. Not
/// what Docker assigned (address, endpoint), and not the old container's
/// own id, which older Docker lists among the aliases.
fn endpoint_config(old: &Value, endpoint: &Value) -> Value {
    let short = text(old, "/Id").get(..12).unwrap_or("");
    let mut out = Map::new();
    for key in ["IPAMConfig", "Links", "DriverOpts", "GwPriority"] {
        if let Some(v) = endpoint.get(key).filter(|v| !v.is_null()) {
            out.insert(key.into(), v.clone());
        }
    }
    let aliases: Vec<Value> = endpoint
        .get("Aliases")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|a| a.as_str().is_some_and(|a| a != short))
        .cloned()
        .collect();
    if !aliases.is_empty() {
        out.insert("Aliases".into(), Value::Array(aliases));
    }
    Value::Object(out)
}

/// How a new container is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    /// Still starting: not yet healthy, or running for less than a while
    /// without a health check.
    Starting,
    Healthy,
    /// It failed; why, in words.
    Failed(String),
}

/// A container without a health check counts once it has run this long.
pub const STEADY_SECS: i64 = 15;

/// Reads `State` from an inspect. `running_secs` is how long it has run.
pub fn health(container: &Value, running_secs: i64) -> Health {
    let state = container.get("State").unwrap_or(&Value::Null);
    let restarts = container
        .get("RestartCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let running = state.get("Running").and_then(Value::as_bool) == Some(true);
    let restarting = state.get("Restarting").and_then(Value::as_bool) == Some(true);
    let error = text(state, "/Error");
    if !error.is_empty() {
        return Health::Failed(format!("the new container failed: {error}"));
    }
    // Caught between two of Docker's restarts, it runs with exit code 0.
    if restarting || restarts > 0 {
        return Health::Failed("the new container keeps stopping".into());
    }
    if !running {
        let code = state.get("ExitCode").and_then(Value::as_i64).unwrap_or(0);
        return Health::Failed(format!("the new container stopped (exit code {code})"));
    }
    match text(state, "/Health/Status") {
        "healthy" => Health::Healthy,
        "unhealthy" => Health::Failed("the new container is unhealthy".into()),
        "" if running_secs >= STEADY_SECS => Health::Healthy,
        _ => Health::Starting,
    }
}

/// A Swarm service's spec with its tasks on `image`: Swarm then replaces
/// them as the service's update settings say.
pub fn service_spec(spec: &Value, image: &str) -> Option<Value> {
    let mut spec = spec.clone();
    let task = spec.get_mut("TaskTemplate")?.as_object_mut()?;
    let force = task
        .get("ForceUpdate")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .saturating_add(1);
    task.insert("ForceUpdate".into(), json!(force));
    task.get_mut("ContainerSpec")?
        .as_object_mut()?
        .insert("Image".into(), json!(image));
    Some(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "4f2c8d1e9a7b6c5d4e3f2a1b0c9d8e7f6a5b4c3d2e1f0a9b8c7d6e5f4a3b2c1d";

    #[test]
    fn splits_image_references() {
        let r = |repo, tag, digest| ImageRef { repo, tag, digest };
        assert_eq!(
            split_image("ghcr.io/starbooru/starbooru-server:latest"),
            r("ghcr.io/starbooru/starbooru-server", Some("latest"), None)
        );
        assert_eq!(
            split_image("localhost:5110/starbooru-server"),
            r("localhost:5110/starbooru-server", None, None)
        );
        assert_eq!(
            split_image("localhost:5110/starbooru-server:0.1.0-beta.6-vulkan"),
            r(
                "localhost:5110/starbooru-server",
                Some("0.1.0-beta.6-vulkan"),
                None
            )
        );
        assert_eq!(
            split_image("starbooru:local@sha256:abcd"),
            r("starbooru", Some("local"), Some("sha256:abcd"))
        );
    }

    #[test]
    fn latest_stays_latest_and_a_version_moves() {
        let t = |image, version| target(image, None, version);
        let latest = t("ghcr.io/starbooru/starbooru-server:latest", "0.3.0").unwrap();
        assert_eq!(
            latest.reference(),
            "ghcr.io/starbooru/starbooru-server:latest"
        );
        assert!(!latest.pinned);
        assert_eq!(
            t("ghcr.io/starbooru/starbooru-server", "0.3.0")
                .unwrap()
                .tag,
            "latest"
        );
        let gpu = t("ghcr.io/starbooru/starbooru-server:latest-nvidia", "0.3.0").unwrap();
        assert_eq!(gpu.tag, "latest-nvidia");
        let pinned = t("ghcr.io/starbooru/starbooru-server:0.2.0", "0.3.0").unwrap();
        assert_eq!(pinned.tag, "0.3.0");
        assert!(pinned.pinned);
        let beta = t(
            "ghcr.io/starbooru/starbooru-server:0.1.0-beta.6-vulkan",
            "0.2.0",
        )
        .unwrap();
        assert_eq!(beta.tag, "0.2.0-vulkan");
        // The testing override keeps the tag rules.
        let moved = target(
            "ghcr.io/starbooru/starbooru-server:0.2.0",
            Some("localhost:5110/starbooru-server"),
            "0.3.0",
        )
        .unwrap();
        assert_eq!(moved.reference(), "localhost:5110/starbooru-server:0.3.0");
        for local in [
            "starbooru:local",
            "starbooru-server:dev",
            "ghcr.io/starbooru/starbooru-server:latest@sha256:00",
        ] {
            assert!(t(local, "0.3.0").is_err(), "{local}");
        }
    }

    #[test]
    fn finds_its_own_container() {
        let mountinfo = format!(
            "1291 1270 0:94 / / rw,relatime master:520 - overlay overlay rw,lowerdir=/var/lib/docker/overlay2/l/X\n\
             1302 1291 259:2 /var/lib/docker/containers/{ID}/resolv.conf /etc/resolv.conf rw,relatime - ext4 /dev/nvme0n1p2 rw\n"
        );
        assert_eq!(
            own_id(&mountinfo, "0::/\n", "4f2c8d1e9a7b"),
            Some(ID.into())
        );
        // Podman's folder, cgroup v1, and the hostname last.
        let podman = format!(
            "12 1 0:5 /containers/storage/overlay-containers/{ID}/userdata/hostname /etc/hostname rw - tmpfs tmpfs rw\n"
        );
        assert_eq!(own_id(&podman, "", ""), Some(ID.into()));
        assert_eq!(
            own_id("", &format!("12:memory:/docker/{ID}\n"), ""),
            Some(ID.into())
        );
        assert_eq!(
            own_id("", "0::/system.slice/docker-x.scope", "4f2c8d1e9a7b\n"),
            Some("4f2c8d1e9a7b".into())
        );
        assert_eq!(own_id("", "0::/\n", "booru"), None);
        assert_eq!(own_id("", "0::/user.slice\n", "my-laptop"), None);
    }

    /// A container as Docker 29 describes one that Compose made.
    fn compose_container() -> Value {
        json!({
            "Id": ID,
            "Name": "/sbtest-compose-starbooru-1",
            "Image": "sha256:1111",
            "Mounts": [
                { "Type": "volume", "Name": "sbtest-compose_media", "Source": "/var/lib/docker/volumes/sbtest-compose_media/_data",
                  "Destination": "/data", "Driver": "local", "Mode": "z", "RW": true },
                { "Type": "volume", "Name": "0e1f2a", "Source": "/var/lib/docker/volumes/0e1f2a/_data",
                  "Destination": "/cache", "Driver": "local", "Mode": "", "RW": true },
                { "Type": "bind", "Source": "/var/run/docker.sock", "Destination": "/var/run/docker.sock", "RW": true }
            ],
            "Config": {
                "Hostname": "4f2c8d1e9a7b",
                "User": "starbooru",
                "ExposedPorts": { "8080/tcp": {}, "9090/tcp": {} },
                "Env": ["DATABASE_URL=postgres://db/starbooru", "STARBOORU_BIND=0.0.0.0:8080", "PATH=/usr/bin"],
                "Cmd": null,
                "Healthcheck": { "Test": ["CMD", "/app/starbooru-server", "healthcheck"] },
                "Image": "localhost:5110/starbooru-server:latest",
                "Volumes": { "/data": {}, "/cache": {} },
                "Entrypoint": ["/app/starbooru-server"],
                "Labels": {
                    "com.docker.compose.image": "sha256:1111",
                    "com.docker.compose.project": "sbtest-compose",
                    "com.docker.compose.service": "starbooru",
                    "traefik.http.routers.booru.rule": "Host(`booru.example.com`)",
                    "org.opencontainers.image.version": "0.2.0"
                }
            },
            "HostConfig": {
                "Binds": ["/var/run/docker.sock:/var/run/docker.sock:rw"],
                "NetworkMode": "sbtest-compose_default",
                "PortBindings": { "8080/tcp": [{ "HostIp": "", "HostPort": "18110" }] },
                "RestartPolicy": { "Name": "unless-stopped", "MaximumRetryCount": 0 },
                "GroupAdd": ["984"],
                "Mounts": [{ "Type": "volume", "Source": "sbtest-compose_media", "Target": "/data" }],
                "DeviceRequests": [{ "Driver": "nvidia", "Count": -1, "Capabilities": [["gpu"]] }]
            },
            "NetworkSettings": {
                "Networks": {
                    "sbtest-compose_default": {
                        "IPAMConfig": null,
                        "Links": null,
                        "Aliases": ["sbtest-compose-starbooru-1", "starbooru"],
                        "MacAddress": "02:42:ac:11:00:02",
                        "NetworkID": "abc",
                        "EndpointID": "def",
                        "IPAddress": "172.18.0.3",
                        "DNSNames": ["sbtest-compose-starbooru-1", "starbooru", "4f2c8d1e9a7b"]
                    },
                    "dokploy-network": {
                        "Aliases": ["4f2c8d1e9a7b"],
                        "IPAMConfig": { "IPv4Address": "10.0.1.20" }
                    }
                }
            }
        })
    }

    fn image() -> Value {
        json!({
            "Id": "sha256:1111",
            "Config": {
                "User": "starbooru",
                "ExposedPorts": { "8080/tcp": {} },
                "Env": ["STARBOORU_BIND=0.0.0.0:8080", "PATH=/usr/bin"],
                "Cmd": null,
                "Healthcheck": { "Test": ["CMD", "/app/starbooru-server", "healthcheck"] },
                "Volumes": { "/data": {}, "/cache": {} },
                "Entrypoint": ["/app/starbooru-server"],
                "Labels": { "org.opencontainers.image.version": "0.2.0" }
            }
        })
    }

    #[test]
    fn a_copy_keeps_what_the_container_set() {
        let body = create_body(
            &compose_container(),
            &image(),
            "localhost:5110/starbooru-server:latest",
            "sha256:2222",
        );
        assert_eq!(body["Image"], "localhost:5110/starbooru-server:latest");
        // The image's own values give way to the new image's.
        for gone in [
            "Hostname",
            "User",
            "Healthcheck",
            "Entrypoint",
            "Cmd",
            "MacAddress",
        ] {
            assert!(body.get(gone).is_none(), "{gone}");
        }
        assert_eq!(body["Env"], json!(["DATABASE_URL=postgres://db/starbooru"]));
        assert_eq!(body["ExposedPorts"], json!({ "9090/tcp": {} }));
        assert_eq!(body["Volumes"], json!({}));
        assert_eq!(
            body["Labels"],
            json!({
                "com.docker.compose.image": "sha256:2222",
                "com.docker.compose.project": "sbtest-compose",
                "com.docker.compose.service": "starbooru",
                "traefik.http.routers.booru.rule": "Host(`booru.example.com`)"
            })
        );
        let host = &body["HostConfig"];
        assert_eq!(host["RestartPolicy"]["Name"], "unless-stopped");
        assert_eq!(host["PortBindings"]["8080/tcp"][0]["HostPort"], "18110");
        assert_eq!(host["GroupAdd"], json!(["984"]));
        assert_eq!(host["DeviceRequests"][0]["Driver"], "nvidia");
        assert_eq!(
            host["Binds"],
            json!(["/var/run/docker.sock:/var/run/docker.sock:rw"])
        );
        // The named volume once, and the anonymous one kept.
        assert_eq!(
            host["Mounts"],
            json!([
                { "Type": "volume", "Source": "sbtest-compose_media", "Target": "/data" },
                { "Type": "volume", "Source": "0e1f2a", "Target": "/cache", "ReadOnly": false }
            ])
        );
        assert_eq!(
            body["NetworkingConfig"],
            json!({ "EndpointsConfig": { "sbtest-compose_default": {
                "Aliases": ["sbtest-compose-starbooru-1", "starbooru"]
            } } })
        );
        assert_eq!(
            extra_networks(&compose_container()),
            vec![(
                "dokploy-network".to_owned(),
                json!({ "IPAMConfig": { "IPv4Address": "10.0.1.20" } })
            )]
        );
    }

    #[test]
    fn values_the_container_changed_stay() {
        let mut old = compose_container();
        old["Config"]["Hostname"] = json!("booru");
        old["Config"]["User"] = json!("0");
        old["Config"]["Cmd"] = json!(["--verbose"]);
        old["HostConfig"]["NetworkMode"] = json!("host");
        let body = create_body(&old, &image(), "x:1", "sha256:2");
        assert_eq!(body["Hostname"], "booru");
        assert_eq!(body["User"], "0");
        assert_eq!(body["Cmd"], json!(["--verbose"]));
        assert!(body.get("NetworkingConfig").is_none());
        assert!(extra_networks(&old).is_empty());
        // Without Compose there is no image label to keep up to date.
        let mut plain = compose_container();
        plain["Config"]["Labels"] = json!({ "traefik.enable": "true" });
        let body = create_body(&plain, &image(), "x:1", "sha256:2");
        assert_eq!(body["Labels"], json!({ "traefik.enable": "true" }));
    }

    #[test]
    fn reads_how_a_new_container_does() {
        let c = |state: Value, restarts: u64| json!({ "State": state, "RestartCount": restarts });
        let healthy = json!({ "Running": true, "Health": { "Status": "healthy" } });
        assert_eq!(health(&c(healthy.clone(), 0), 1), Health::Healthy);
        assert_eq!(
            health(
                &c(
                    json!({ "Running": true, "Health": { "Status": "starting" } }),
                    0
                ),
                60
            ),
            Health::Starting
        );
        assert!(matches!(
            health(
                &c(
                    json!({ "Running": true, "Health": { "Status": "unhealthy" } }),
                    0
                ),
                5
            ),
            Health::Failed(_)
        ));
        assert_eq!(
            health(&c(json!({ "Running": false, "ExitCode": 1 }), 0), 0),
            Health::Failed("the new container stopped (exit code 1)".into())
        );
        // Docker restarted it: it died once already.
        assert_eq!(
            health(&c(healthy, 1), 30),
            Health::Failed("the new container keeps stopping".into())
        );
        assert_eq!(
            health(&c(json!({ "Restarting": true, "ExitCode": 1 }), 0), 3),
            Health::Failed("the new container keeps stopping".into())
        );
        assert_eq!(
            health(
                &c(
                    json!({ "Running": false, "ExitCode": 127, "Error": "exec: \"/x\": not found" }),
                    0
                ),
                1
            ),
            Health::Failed("the new container failed: exec: \"/x\": not found".into())
        );
        // No health check: running for a while is enough.
        let plain = c(json!({ "Running": true }), 0);
        assert_eq!(health(&plain, 2), Health::Starting);
        assert_eq!(health(&plain, STEADY_SECS), Health::Healthy);
    }

    #[test]
    fn a_service_gets_the_new_image() {
        let spec = json!({
            "Name": "booru_starbooru",
            "TaskTemplate": {
                "ContainerSpec": { "Image": "ghcr.io/starbooru/starbooru-server:latest@sha256:abc", "Env": ["A=1"] },
                "ForceUpdate": 2
            },
            "Labels": { "com.docker.stack.namespace": "booru" }
        });
        let next = service_spec(&spec, "ghcr.io/starbooru/starbooru-server:latest").unwrap();
        assert_eq!(
            next["TaskTemplate"]["ContainerSpec"]["Image"],
            "ghcr.io/starbooru/starbooru-server:latest"
        );
        assert_eq!(next["TaskTemplate"]["ForceUpdate"], 3);
        assert_eq!(next["TaskTemplate"]["ContainerSpec"]["Env"], json!(["A=1"]));
        assert_eq!(next["Labels"], spec["Labels"]);
        assert_eq!(service_spec(&json!({}), "x:1"), None);
    }
}
