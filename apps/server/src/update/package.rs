//! Updating the server package: a plain binary that a service manager
//! starts again when it exits. The new release's tarball is checked against
//! the release's `SHA256SUMS`, its binary unpacked beside the running one,
//! asked for its version, and renamed over it. The old binary stays as
//! `starbooru-server.old`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};
use starbooru_core::UpdateBlock;
use tokio::io::AsyncWriteExt;

use super::github::{Asset, Found, read_capped};

/// The release's tarball is about 8 MB.
const MAX_PACKAGE_BYTES: u64 = 512 << 20;
const MAX_SUMS_BYTES: usize = 64 << 10;
const BINARY: &str = "starbooru-server";
/// The new binary has this long to say its version.
const VERSION_TIMEOUT: Duration = Duration::from_secs(15);

/// Whether this binary can replace itself: a release build on Linux, in a
/// folder it can write to, under a service manager that starts it again.
pub fn judge(
    release_build: bool,
    os: &str,
    folder_writable: bool,
    supervised: bool,
) -> Result<(), (UpdateBlock, &'static str)> {
    if !release_build {
        return Err((
            UpdateBlock::DevelopmentBuild,
            "this server was built from source; update the source and build it again",
        ));
    }
    if os != "linux" {
        return Err((
            UpdateBlock::Unsupported,
            "server packages are made for Linux only",
        ));
    }
    if !folder_writable {
        return Err((
            UpdateBlock::ReadOnlyFolder,
            "the server can't write to the folder its binary is in",
        ));
    }
    if !supervised {
        return Err((
            UpdateBlock::NotSupervised,
            "no service manager would start the server again; run it as a systemd service (see the deployment guide)",
        ));
    }
    Ok(())
}

/// Whether the server may write to `dir`, found by trying.
pub fn writable(dir: &Path) -> bool {
    let probe = dir.join(".starbooru-write-test");
    let made = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    made
}

/// The package for this machine in a release.
pub fn package_name(version: &str, arch: &str) -> String {
    format!("{BINARY}-{version}-linux-{arch}.tar.gz")
}

/// The SHA-256 `SHA256SUMS` gives for `name` (`<hex>  <name>`, or `*<name>`
/// for a binary-mode line).
pub fn sum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let (hash, file) = (parts.next()?, parts.next()?);
        (file.trim_start_matches('*') == name
            && hash.len() == 64
            && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| hash.to_ascii_lowercase())
    })
}

/// The binary's path next to `exe` with `suffix` added.
pub fn beside(exe: &Path, suffix: &str) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    exe.with_file_name(name)
}

/// Downloads release `to` for this machine, checks it and unpacks its
/// binary as `<exe>.new`, which says it is `to`. Returns its path.
pub async fn fetch(
    client: &reqwest::Client,
    found: &Found,
    to: &str,
    exe: &Path,
) -> Result<PathBuf, String> {
    let name = package_name(to, std::env::consts::ARCH);
    let package = found.asset(&name).ok_or_else(|| {
        format!(
            "this release has no server package for linux-{}",
            std::env::consts::ARCH
        )
    })?;
    let sums = found
        .asset("SHA256SUMS")
        .ok_or("this release has no SHA256SUMS to check the package against")?;
    let sums = client
        .get(&sums.url)
        .send()
        .await
        .map_err(|_| "the release's SHA256SUMS can't be downloaded".to_owned())?;
    if !sums.status().is_success() {
        return Err(format!(
            "the release's SHA256SUMS can't be downloaded: the server answered {}",
            sums.status().as_u16()
        ));
    }
    let sums = read_capped(sums, MAX_SUMS_BYTES).await?;
    let expected = sum_for(&String::from_utf8_lossy(&sums), &name)
        .ok_or_else(|| format!("SHA256SUMS doesn't list {name}"))?;

    let tarball = beside(exe, ".download");
    let result = async {
        let digest = download(client, package, &tarball).await?;
        if digest != expected {
            return Err(format!("{name} doesn't match its SHA-256 in SHA256SUMS"));
        }
        let new = beside(exe, ".new");
        let (from, to_path) = (tarball.clone(), new.clone());
        tokio::task::spawn_blocking(move || unpack(&from, &to_path))
            .await
            .map_err(|e| e.to_string())??;
        if let Err(e) = says_version(&new, to).await {
            let _ = tokio::fs::remove_file(&new).await;
            return Err(e);
        }
        Ok(new)
    }
    .await;
    let _ = tokio::fs::remove_file(&tarball).await;
    result
}

/// Streams `asset` to `dest`, returning its SHA-256.
async fn download(client: &reqwest::Client, asset: &Asset, dest: &Path) -> Result<String, String> {
    let cut = |_| "the package's download was cut short".to_owned();
    let mut resp = client
        .get(&asset.url)
        .send()
        .await
        .map_err(|_| "the package can't be downloaded: check the server's internet access")?;
    if !resp.status().is_success() {
        return Err(format!(
            "the package can't be downloaded: the server answered {}",
            resp.status().as_u16()
        ));
    }
    let mut file = tokio::fs::File::create(dest).await.map_err(write_error)?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    while let Some(chunk) = resp.chunk().await.map_err(cut)? {
        size = size.saturating_add(chunk.len() as u64);
        if size > MAX_PACKAGE_BYTES {
            return Err("the package is far larger than a server package".to_owned());
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(write_error)?;
    }
    file.flush().await.map_err(write_error)?;
    Ok(hex::encode(hasher.finalize()))
}

fn write_error(e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::StorageFull {
        "the disk is full".to_owned()
    } else {
        format!("the package can't be saved beside the server: {e}")
    }
}

/// Copies `<folder>/starbooru-server` out of the tarball to `dest`, as an
/// executable.
fn unpack(tarball: &Path, dest: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let file = std::fs::File::open(tarball).map_err(|e| e.to_string())?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let unreadable = |_| "the package can't be read".to_owned();
    for entry in archive.entries().map_err(unreadable)? {
        let mut entry = entry.map_err(unreadable)?;
        let path = entry.path().map_err(unreadable)?.into_owned();
        let mut parts = path.components();
        let is_binary = matches!(
            (parts.next(), parts.next(), parts.next()),
            (Some(std::path::Component::Normal(_)), Some(std::path::Component::Normal(file)), None)
                if file == BINARY
        );
        if !is_binary || !entry.header().entry_type().is_file() {
            continue;
        }
        if entry.size() > MAX_PACKAGE_BYTES {
            return Err("the package's binary is too large".to_owned());
        }
        let mut out = std::fs::File::create(dest).map_err(write_error)?;
        std::io::copy(&mut entry.by_ref().take(MAX_PACKAGE_BYTES), &mut out)
            .map_err(write_error)?;
        out.sync_all().map_err(write_error)?;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))
            .map_err(write_error)?;
        return Ok(());
    }
    Err(format!("the package holds no {BINARY}"))
}

/// Runs `binary version`, which must print `to`: it runs on this machine
/// and it is the release it should be.
async fn says_version(binary: &Path, to: &str) -> Result<(), String> {
    let mut tries = 0;
    let out = loop {
        let run = tokio::process::Command::new(binary)
            .arg("version")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        match tokio::time::timeout(VERSION_TIMEOUT, run).await {
            Err(_) => return Err("the new binary didn't answer".to_owned()),
            Ok(Ok(out)) => break out,
            // Another process forked while the file was still open for writing.
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && tries < 10 => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Ok(Err(e)) => return Err(format!("the new binary doesn't run here: {e}")),
        }
    };
    let said = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if !out.status.success() || said != to {
        return Err(format!(
            "the new binary says it is {}, not {to}",
            if said.is_empty() { "nothing" } else { &said }
        ));
    }
    Ok(())
}

/// Puts `new` in place of `exe`, keeping `exe` as `<exe>.old`. The rename
/// is atomic, so `exe` is always one whole binary or the other.
pub fn swap(exe: &Path, new: &Path) -> std::io::Result<()> {
    let old = beside(exe, ".old");
    match std::fs::remove_file(&old) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if std::fs::hard_link(exe, &old).is_err() {
        std::fs::copy(exe, &old)?;
    }
    std::fs::rename(new, exe)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Write;

    use super::*;

    /// A tarball laid out as `release.yml` packs one, whose binary is a
    /// script that says `version`.
    pub fn tarball(version: &str, says: &str) -> Vec<u8> {
        let folder = format!("{BINARY}-{version}-linux-{}", std::env::consts::ARCH);
        let mut tar = tar::Builder::new(Vec::new());
        for (name, body, mode) in [
            (
                BINARY,
                format!("#!/bin/sh\necho {says}\n").into_bytes(),
                0o755,
            ),
            ("LICENSE", b"AGPL".to_vec(), 0o644),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(mode);
            header.set_cksum();
            tar.append_data(&mut header, format!("{folder}/{name}"), body.as_slice())
                .unwrap();
        }
        let tar = tar.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tar).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn only_a_supervised_release_in_its_own_folder_updates_itself() {
        assert!(judge(true, "linux", true, true).is_ok());
        let reason = |r: Result<(), (UpdateBlock, &str)>| r.unwrap_err().0;
        assert_eq!(
            reason(judge(false, "linux", true, true)),
            UpdateBlock::DevelopmentBuild
        );
        assert_eq!(
            reason(judge(true, "windows", true, true)),
            UpdateBlock::Unsupported
        );
        assert_eq!(
            reason(judge(true, "linux", false, true)),
            UpdateBlock::ReadOnlyFolder
        );
        assert_eq!(
            reason(judge(true, "linux", true, false)),
            UpdateBlock::NotSupervised
        );
        let dir = tempfile::tempdir().unwrap();
        assert!(writable(dir.path()));
        assert!(!writable(&dir.path().join("missing")));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn reads_sha256sums() {
        let hash = "6f52f67e16182295e3fc33267fe29d4d274c9f9c270acf0e116dd4106adb29b7";
        let sums = format!(
            "{hash}  starbooru-server-0.2.0-linux-x86_64.tar.gz\n\
             {}  *starbooru-server-0.2.0-linux-aarch64.tar.gz\n",
            hash.to_uppercase()
        );
        assert_eq!(
            sum_for(&sums, "starbooru-server-0.2.0-linux-x86_64.tar.gz").as_deref(),
            Some(hash)
        );
        assert_eq!(
            sum_for(&sums, "starbooru-server-0.2.0-linux-aarch64.tar.gz").as_deref(),
            Some(hash)
        );
        assert_eq!(sum_for(&sums, "starbooru-server-0.2.0.tar.gz"), None);
        assert_eq!(sum_for("nothash  x.tar.gz", "x.tar.gz"), None);
        assert_eq!(
            package_name("0.3.0", "x86_64"),
            "starbooru-server-0.3.0-linux-x86_64.tar.gz"
        );
    }

    #[test]
    fn unpacks_the_binary_and_swaps_it_in() {
        let dir = tempfile::tempdir().unwrap();
        let tarball_path = dir.path().join("p.tar.gz");
        std::fs::write(&tarball_path, tarball("0.3.0", "0.3.0")).unwrap();
        let exe = dir.path().join(BINARY);
        std::fs::write(&exe, b"old binary").unwrap();
        let new = beside(&exe, ".new");
        unpack(&tarball_path, &new).unwrap();
        assert!(std::fs::read(&new).unwrap().starts_with(b"#!/bin/sh"));
        swap(&exe, &new).unwrap();
        assert!(std::fs::read(&exe).unwrap().starts_with(b"#!/bin/sh"));
        assert_eq!(std::fs::read(beside(&exe, ".old")).unwrap(), b"old binary");
        assert!(!new.exists());
        // A second update replaces the kept one.
        unpack(&tarball_path, &new).unwrap();
        swap(&exe, &new).unwrap();
        assert!(
            std::fs::read(beside(&exe, ".old"))
                .unwrap()
                .starts_with(b"#!/bin/sh")
        );

        let empty = dir.path().join("empty.tar.gz");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tar::Builder::new(Vec::new()).into_inner().unwrap())
            .unwrap();
        std::fs::write(&empty, gz.finish().unwrap()).unwrap();
        assert!(unpack(&empty, &dir.path().join("x")).is_err());
        std::fs::write(&empty, b"not a tarball").unwrap();
        assert!(unpack(&empty, &dir.path().join("x")).is_err());
    }

    #[tokio::test]
    async fn the_new_binary_must_say_its_version() {
        let dir = tempfile::tempdir().unwrap();
        let tarball_path = dir.path().join("p.tar.gz");
        std::fs::write(&tarball_path, tarball("0.3.0", "0.2.9")).unwrap();
        let new = dir.path().join("starbooru-server.new");
        unpack(&tarball_path, &new).unwrap();
        assert_eq!(
            says_version(&new, "0.3.0").await.unwrap_err(),
            "the new binary says it is 0.2.9, not 0.3.0"
        );
        assert!(says_version(&new, "0.2.9").await.is_ok());
        assert!(
            says_version(&dir.path().join("missing"), "0.3.0")
                .await
                .is_err()
        );
    }
}
