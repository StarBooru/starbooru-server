//! Downloads the built-in models into `<data>/ai/models/<component>/`.
//!
//! Every file is pinned to a commit and a SHA-256. A download resumes where
//! it stopped and a component only counts once all its files are verified.
//! The AI runtime comes the same way when it isn't next to the worker, as it
//! is in the Docker image.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use sha2::{Digest, Sha256};
use starbooru_core::ai_catalog::{Component, ModelFile};
use tokio::io::AsyncWriteExt;
use tracing::warn;

use crate::error::{AppError, AppResult};

const MARKER: &str = ".complete";
/// A big model on a slow line takes hours.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(24 * 3600);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = concat!("StarBooru/", env!("CARGO_PKG_VERSION"));

/// The ONNX Runtime release, the same as in the `Dockerfile`: Microsoft's
/// NuGet package, which holds the library for both processors.
const RUNTIME_VERSION: &str = "1.31.1";
const RUNTIME_PACKAGE: &str = "microsoft.ml.onnxruntime";
const RUNTIME_PACKAGE_SHA256: &str =
    "6871ccbc97f3d5bdb4095a1dcec33aa78d56f6ed8de5980ef5fa1d9f2e7c679a";
const RUNTIME_PACKAGE_SIZE: u64 = 159_626_394;

struct RuntimePin {
    os: &'static str,
    arch: &'static str,
    library: Member,
}

/// A file in a package: its path there, and what it unpacks to.
struct Member {
    path: &'static str,
    sha256: &'static str,
    size: u64,
}

const RUNTIMES: &[RuntimePin] = &[
    RuntimePin {
        os: "linux",
        arch: "x86_64",
        library: Member {
            path: "runtimes/linux-x64/native/libonnxruntime.so",
            sha256: "a96f132e5eb5ca04e23a7d2fc1078fb95aa591e8d71d0f237702ece3a1e0f4ea",
            size: 29_345_384,
        },
    },
    RuntimePin {
        os: "linux",
        arch: "aarch64",
        library: Member {
            path: "runtimes/linux-arm64/native/libonnxruntime.so",
            sha256: "7e17c55d675be73f67a218dbecb55522395f61dafb3da03ebc7ade56a4c2f813",
            size: 25_266_408,
        },
    },
];

/// Where the runtime goes when it has to be downloaded.
pub fn runtime_library(data_dir: &Path) -> PathBuf {
    data_dir
        .join("ai")
        .join("runtime")
        .join(format!("libonnxruntime-{RUNTIME_VERSION}.so"))
}

/// Makes sure `library` is the pinned runtime, downloading it when it's missing
/// or damaged. Takes a user-readable reason when it can't.
pub async fn install_runtime(library: &Path) -> Result<(), String> {
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    let Some(pin) = RUNTIMES.iter().find(|r| r.os == os && r.arch == arch) else {
        return Err(format!(
            "no AI runtime for {os} on {arch}: set STARBOORU_ORT_LIB to an ONNX Runtime {RUNTIME_VERSION} library"
        ));
    };
    let current = sha256_of(library.to_path_buf()).await;
    if current.is_ok_and(|hash| hash == pin.library.sha256) {
        return Ok(());
    }
    let package = Package {
        url: format!(
            "https://api.nuget.org/v3-flatcontainer/{RUNTIME_PACKAGE}/{RUNTIME_VERSION}/{RUNTIME_PACKAGE}.{RUNTIME_VERSION}.nupkg"
        ),
        sha256: RUNTIME_PACKAGE_SHA256.to_owned(),
        size: RUNTIME_PACKAGE_SIZE,
    };
    fetch_runtime(&package, &pin.library, library)
        .await
        .map_err(|e| {
            warn!("downloading the AI runtime failed: {e}");
            "the AI runtime can't be downloaded: check the server's internet access and free disk space"
                .to_owned()
        })
}

/// A pinned ZIP to take the runtime from.
struct Package {
    url: String,
    sha256: String,
    size: u64,
}

/// Puts `member` of the package at `library`, checked against its size and
/// hash. Only the library's bytes are fetched when the server sends ranges
/// (12 MB of a 160 MB package); otherwise the whole package, checked
/// against its own hash.
async fn fetch_runtime(package: &Package, member: &Member, library: &Path) -> AppResult<()> {
    let dir = library
        .parent()
        .ok_or_else(|| AppError::Internal("the AI runtime has no folder".into()))?;
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| write_error(&e))?;
    let unpacked = dir.join("library.part");
    let ranged = match fetch_member(package, member, &unpacked).await {
        Ok(found) => found,
        Err(e) => {
            warn!("reading the AI runtime in parts failed, downloading all of it: {e}");
            false
        }
    };
    let ranged = ranged && sha256_of(unpacked.clone()).await? == member.sha256;
    if !ranged {
        let archive = dir.join("package.zip");
        let part = dir.join("package.zip.part");
        download(
            &package.url,
            &archive,
            &part,
            &package.sha256,
            package.size,
            |_| {},
        )
        .await?;
        let (from, to, name, size) = (
            archive.clone(),
            unpacked.clone(),
            member.path.to_owned(),
            member.size,
        );
        let found = tokio::task::spawn_blocking(move || unzip_file(&from, &name, size, &to))
            .await
            .map_err(AppError::internal)?
            .map_err(|e| write_error(&e));
        let _ = tokio::fs::remove_file(&archive).await;
        if !found? || sha256_of(unpacked.clone()).await? != member.sha256 {
            let _ = tokio::fs::remove_file(&unpacked).await;
            return Err(AppError::Upstream(
                "the AI runtime is not the expected one".into(),
            ));
        }
    }
    tokio::fs::rename(&unpacked, library)
        .await
        .map_err(|e| write_error(&e))
}

/// The end of a ZIP file holds its directory's place, in the last 64 KiB.
const ZIP_TAIL: u64 = 22 + 65_535;
/// More directory than a package of a few dozen files has.
const MAX_ZIP_DIRECTORY: u32 = 16 * 1024 * 1024;
/// More than the runtime library takes packed.
const MAX_PACKED: u64 = 256 * 1024 * 1024;
const LOCAL_HEADER: u64 = 30;

fn le16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// The offset and size of a ZIP's directory, from the end of the file.
fn zip_directory(tail: &[u8]) -> Option<(u64, u32)> {
    let end = tail
        .windows(4)
        .rposition(|w| w == [0x50, 0x4b, 0x05, 0x06])?;
    let (size, offset) = (le32(tail, end + 12)?, le32(tail, end + 16)?);
    (size <= MAX_ZIP_DIRECTORY).then_some((u64::from(offset), size))
}

/// Where a file's data starts, from its local header.
fn data_start(header: u64, local: &[u8]) -> Option<u64> {
    if le32(local, 0)? != 0x0403_4b50 {
        return None;
    }
    let skip = u64::from(le16(local, 26)?) + u64::from(le16(local, 28)?);
    header.checked_add(LOCAL_HEADER)?.checked_add(skip)
}

/// Where a file is in a ZIP: its compression, packed size and the offset of
/// its local header.
fn zip_entry(directory: &[u8], member: &str) -> Option<(u16, u64, u64)> {
    let mut at = 0usize;
    while le32(directory, at)? == 0x0201_4b50 {
        let method = le16(directory, at + 10)?;
        let packed = le32(directory, at + 20)?;
        let name_len = usize::from(le16(directory, at + 28)?);
        let extra_len = usize::from(le16(directory, at + 30)?);
        let comment_len = usize::from(le16(directory, at + 32)?);
        let offset = le32(directory, at + 42)?;
        let name = directory.get(at + 46..at + 46 + name_len)?;
        if name == member.as_bytes() {
            // Over 4 GiB is ZIP64, which a runtime package isn't.
            if packed == u32::MAX || offset == u32::MAX || u64::from(packed) > MAX_PACKED {
                return None;
            }
            return Some((method, u64::from(packed), u64::from(offset)));
        }
        at = at.checked_add(46 + name_len + extra_len + comment_len)?;
    }
    None
}

/// Writes a ZIP member's packed bytes out, stored or deflated, and at most
/// `size` of them: the packed bytes of a ranged read aren't checked before
/// they are unpacked, and a few could inflate to fill the disk. Whether it
/// knew the compression and the member kept to its size.
fn unpack(method: u16, data: impl Read, size: u64, dest: &Path) -> std::io::Result<bool> {
    let cap = size.saturating_add(1);
    let mut out = std::fs::File::create(dest)?;
    let written = match method {
        0 => std::io::copy(&mut data.take(cap), &mut out)?,
        8 => std::io::copy(
            &mut flate2::read::DeflateDecoder::new(data).take(cap),
            &mut out,
        )?,
        _ => return Ok(false),
    };
    if written > size {
        drop(out);
        let _ = std::fs::remove_file(dest);
        return Ok(false);
    }
    Ok(true)
}

/// Bytes `from..to` of `url`, when the server sends just those.
async fn range(url: &str, from: u64, to: u64) -> AppResult<Option<Vec<u8>>> {
    let Some(last) = to.checked_sub(1).filter(|&last| last >= from) else {
        return Ok(Some(Vec::new()));
    };
    let mut resp = client()?
        .get(url)
        .header(reqwest::header::RANGE, format!("bytes={from}-{last}"))
        .send()
        .await
        .map_err(network_error)?;
    if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Ok(None);
    }
    let want = usize::try_from(to - from).map_err(AppError::internal)?;
    let mut out = Vec::with_capacity(want);
    while let Some(chunk) = resp.chunk().await.map_err(network_error)? {
        if out.len() + chunk.len() > want {
            return Ok(None);
        }
        out.extend_from_slice(&chunk);
    }
    Ok((out.len() == want).then_some(out))
}

/// Copies one file out of a ZIP on a server that sends ranges to `dest`,
/// fetching only its directory and that file. Whether it could.
async fn fetch_member(package: &Package, member: &Member, dest: &Path) -> AppResult<bool> {
    let (full_size, member) = (member.size, member.path);
    let url = package.url.as_str();
    let tail_start = package.size.saturating_sub(ZIP_TAIL);
    let Some(tail) = range(url, tail_start, package.size).await? else {
        return Ok(false);
    };
    let Some((offset, size)) = zip_directory(&tail) else {
        return Ok(false);
    };
    let end = offset.saturating_add(u64::from(size));
    let directory = match offset.checked_sub(tail_start) {
        Some(at) => usize::try_from(at)
            .ok()
            .and_then(|at| tail.get(at..at.checked_add(usize::try_from(size).ok()?)?))
            .map(<[u8]>::to_vec),
        None => range(url, offset, end).await?,
    };
    let Some((method, packed, header)) = directory.and_then(|d| zip_entry(&d, member)) else {
        return Ok(false);
    };
    let local = range(url, header, header.saturating_add(LOCAL_HEADER)).await?;
    let Some(start) = local.and_then(|l| data_start(header, &l)) else {
        return Ok(false);
    };
    let Some(data) = range(url, start, start.saturating_add(packed)).await? else {
        return Ok(false);
    };
    let dest = dest.to_path_buf();
    tokio::task::spawn_blocking(move || unpack(method, data.as_slice(), full_size, &dest))
        .await
        .map_err(AppError::internal)?
        .map_err(|e| write_error(&e))
}

/// Copies one file out of a ZIP (a NuGet package) to `dest`, stored or
/// deflated, at most `full_size` bytes of it. Whether it was there, at
/// most that size.
fn unzip_file(archive: &Path, member: &str, full_size: u64, dest: &Path) -> std::io::Result<bool> {
    use std::io::{Seek, SeekFrom};
    let mut file = std::fs::File::open(archive)?;
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(ZIP_TAIL)))?;
    let mut tail = Vec::new();
    file.by_ref().take(ZIP_TAIL).read_to_end(&mut tail)?;
    let Some((offset, size)) = zip_directory(&tail) else {
        return Ok(false);
    };
    file.seek(SeekFrom::Start(offset))?;
    let mut directory = Vec::new();
    file.by_ref()
        .take(u64::from(size))
        .read_to_end(&mut directory)?;
    let Some((method, packed, header)) = zip_entry(&directory, member) else {
        return Ok(false);
    };
    file.seek(SeekFrom::Start(header))?;
    let mut local = [0u8; LOCAL_HEADER as usize];
    file.read_exact(&mut local)?;
    let Some(start) = data_start(header, &local) else {
        return Ok(false);
    };
    file.seek(SeekFrom::Start(start))?;
    unpack(method, file.take(packed), full_size, dest)
}

pub fn component_dir(models: &Path, component: &Component) -> PathBuf {
    models.join(component.id)
}

/// A component is there when its marker is and its files have the right size.
pub fn is_installed(models: &Path, component: &Component) -> bool {
    let dir = component_dir(models, component);
    dir.join(MARKER).is_file()
        && component
            .files
            .iter()
            .all(|f| std::fs::metadata(dir.join(f.name())).is_ok_and(|m| m.len() == f.size))
}

/// Bytes of a component already on disk, for showing progress.
pub fn bytes_present(models: &Path, component: &Component) -> u64 {
    let dir = component_dir(models, component);
    component
        .files
        .iter()
        .map(|f| {
            std::fs::metadata(dir.join(f.name()))
                .or_else(|_| std::fs::metadata(part_path(&dir, f)))
                .map_or(0, |m| m.len())
        })
        .sum()
}

fn part_path(dir: &Path, file: &ModelFile) -> PathBuf {
    dir.join(format!("{}.part", file.name()))
}

pub async fn remove(models: &Path, component: &Component) -> AppResult<()> {
    let dir = component_dir(models, component);
    match tokio::fs::remove_dir_all(&dir).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(AppError::internal(e)),
    }
}

fn client() -> AppResult<&'static reqwest::Client> {
    static CLIENT: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            // sqlx already uses ring; Err means a provider is installed already.
            let _ = rustls::crypto::ring::default_provider().install_default();
            reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(DOWNLOAD_TIMEOUT)
                .build()
                .map_err(|e| warn!("cannot start the model download client: {e}"))
                .ok()
        })
        .as_ref()
        .ok_or_else(|| AppError::Internal("the model download client failed to start".into()))
}

fn write_error(e: &std::io::Error) -> AppError {
    if e.kind() == std::io::ErrorKind::StorageFull {
        AppError::Upstream("not enough disk space for this model".into())
    } else {
        AppError::Upstream("the model can't be saved".into())
    }
}

fn network_error(_: reqwest::Error) -> AppError {
    AppError::Upstream("the model can't be downloaded: check the server's internet access".into())
}

async fn sha256_of(path: PathBuf) -> AppResult<String> {
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(path).map_err(AppError::internal)?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0; 1 << 20];
        loop {
            let read = file.read(&mut buffer).map_err(AppError::internal)?;
            let Some(chunk) = buffer.get(..read).filter(|c| !c.is_empty()) else {
                break;
            };
            hasher.update(chunk);
        }
        Ok(hex::encode(hasher.finalize()))
    })
    .await
    .map_err(AppError::internal)?
}

/// Checks an installed component against its hashes and removes the files that
/// don't match, so installing it again fetches only those. Whether any didn't.
pub async fn remove_damaged(models: &Path, component: &Component) -> AppResult<bool> {
    let dir = component_dir(models, component);
    let mut damaged = false;
    for file in component.files {
        let path = dir.join(file.name());
        if sha256_of(path.clone()).await? != file.sha256 {
            tokio::fs::remove_file(&path)
                .await
                .map_err(AppError::internal)?;
            damaged = true;
        }
    }
    if damaged {
        tokio::fs::remove_file(dir.join(MARKER))
            .await
            .map_err(AppError::internal)?;
    }
    Ok(damaged)
}

/// Downloads one file to `dest` by way of `<dest>.part`, resuming one left
/// by an earlier attempt. `on_bytes` hears about every chunk.
pub async fn download(
    url: &str,
    dest: &Path,
    part: &Path,
    sha256: &str,
    size: u64,
    mut on_bytes: impl FnMut(u64),
) -> AppResult<()> {
    let have = tokio::fs::metadata(part).await.map_or(0, |m| m.len());
    if have < size {
        let mut req = client()?.get(url);
        if have > 0 {
            req = req.header(reqwest::header::RANGE, format!("bytes={have}-"));
        }
        let mut resp = req.send().await.map_err(network_error)?;
        if !resp.status().is_success() {
            return Err(AppError::Upstream(format!(
                "the model can't be downloaded: the server answered {}",
                resp.status().as_u16()
            )));
        }
        // A server that ignores the range sends the file from the start.
        let resumed = resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
        let mut options = tokio::fs::OpenOptions::new();
        options.create(true);
        if resumed {
            options.append(true);
            on_bytes(have);
        } else {
            options.write(true).truncate(true);
        }
        let mut file = options.open(part).await.map_err(|e| write_error(&e))?;
        let mut written = if resumed { have } else { 0 };
        while let Some(chunk) = resp.chunk().await.map_err(network_error)? {
            written = written.saturating_add(chunk.len() as u64);
            if written > size {
                drop(file);
                let _ = tokio::fs::remove_file(part).await;
                return Err(AppError::Upstream(
                    "the model's file is not the expected one".into(),
                ));
            }
            if let Err(e) = file.write_all(&chunk).await {
                drop(file);
                // Disk full: the partial file only makes it worse.
                let _ = tokio::fs::remove_file(part).await;
                return Err(write_error(&e));
            }
            on_bytes(chunk.len() as u64);
        }
        file.flush().await.map_err(|e| write_error(&e))?;
    } else {
        on_bytes(have);
    }
    if sha256_of(part.to_path_buf()).await? != sha256 {
        let _ = tokio::fs::remove_file(part).await;
        return Err(AppError::Upstream(
            "the model's file is not the expected one".into(),
        ));
    }
    tokio::fs::rename(part, dest)
        .await
        .map_err(|e| write_error(&e))
}

/// Downloads what's missing of a component and marks it as complete.
pub async fn install(
    models: &Path,
    component: &Component,
    mut on_bytes: impl FnMut(u64),
) -> AppResult<()> {
    if is_installed(models, component) {
        return Ok(());
    }
    let dir = component_dir(models, component);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| write_error(&e))?;
    for file in component.files {
        let dest = dir.join(file.name());
        if tokio::fs::metadata(&dest)
            .await
            .is_ok_and(|m| m.len() == file.size)
        {
            on_bytes(file.size);
            continue;
        }
        download(
            &file.url(),
            &dest,
            &part_path(&dir, file),
            file.sha256,
            file.size,
            &mut on_bytes,
        )
        .await?;
    }
    tokio::fs::write(dir.join(MARKER), b"")
        .await
        .map_err(|e| write_error(&e))
}

#[cfg(test)]
pub fn mark_installed(models: &Path, component: &Component) {
    let dir = component_dir(models, component);
    std::fs::create_dir_all(&dir).unwrap();
    for file in component.files {
        let f = std::fs::File::create(dir.join(file.name())).unwrap();
        f.set_len(file.size).unwrap();
    }
    std::fs::write(dir.join(MARKER), b"").unwrap();
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::get;

    use super::*;

    const BODY: &[u8] = b"model file contents, some bytes of them";

    fn digest(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    /// Serves BODY, honouring ranges, and counts what it sent.
    async fn server() -> (String, Arc<AtomicUsize>) {
        let sent = Arc::new(AtomicUsize::new(0));
        let counter = sent.clone();
        let app = Router::new().route(
            "/file",
            get(move |headers: HeaderMap| {
                let counter = counter.clone();
                async move {
                    let from = headers
                        .get("range")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.strip_prefix("bytes="))
                        .and_then(|v| v.strip_suffix('-'))
                        .and_then(|v| v.parse::<usize>().ok());
                    let start = from.unwrap_or(0);
                    let body = BODY.get(start..).unwrap_or_default().to_vec();
                    counter.fetch_add(body.len(), Ordering::SeqCst);
                    let status = if from.is_some() {
                        StatusCode::PARTIAL_CONTENT
                    } else {
                        StatusCode::OK
                    };
                    (status, body)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        (format!("http://{addr}/file"), sent)
    }

    #[tokio::test]
    async fn downloads_and_verifies() {
        let (url, _) = server().await;
        let dir = tempfile::tempdir().unwrap();
        let (dest, part) = (dir.path().join("f"), dir.path().join("f.part"));
        let mut seen = 0;
        download(&url, &dest, &part, &digest(BODY), BODY.len() as u64, |n| {
            seen += n
        })
        .await
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), BODY);
        assert!(!part.exists());
        assert_eq!(seen, BODY.len() as u64);
    }

    #[tokio::test]
    async fn resumes_where_it_stopped() {
        let (url, sent) = server().await;
        let dir = tempfile::tempdir().unwrap();
        let (dest, part) = (dir.path().join("f"), dir.path().join("f.part"));
        std::fs::write(&part, &BODY[..10]).unwrap();
        download(&url, &dest, &part, &digest(BODY), BODY.len() as u64, |_| {})
            .await
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), BODY);
        assert_eq!(sent.load(Ordering::SeqCst), BODY.len() - 10);
    }

    #[tokio::test]
    async fn refuses_a_file_that_is_not_the_pinned_one() {
        let (url, _) = server().await;
        let dir = tempfile::tempdir().unwrap();
        let (dest, part) = (dir.path().join("f"), dir.path().join("f.part"));
        let wrong = digest(b"something else");
        let err = download(&url, &dest, &part, &wrong, BODY.len() as u64, |_| {}).await;
        assert!(err.is_err());
        assert!(!dest.exists());
        // The next attempt starts clean.
        assert!(!part.exists());
        // More bytes than the catalog says stop the download early.
        let err = download(&url, &dest, &part, &digest(BODY), 4, |_| {}).await;
        assert!(err.is_err());
        assert!(!dest.exists() && !part.exists());
    }

    #[tokio::test]
    async fn only_files_that_changed_count_as_damaged() {
        static FILES: [ModelFile; 2] = [
            ModelFile {
                repo: "",
                rev: "",
                path: "good.onnx",
                sha256: "b84207a70c0b96bd7164e7e3cce66dcd9dca497720acf4f71c84f0181aca343e",
                size: 39,
            },
            ModelFile {
                repo: "",
                rev: "",
                path: "bad.onnx",
                sha256: "b84207a70c0b96bd7164e7e3cce66dcd9dca497720acf4f71c84f0181aca343e",
                size: 39,
            },
        ];
        let component = Component {
            id: "test",
            tool: starbooru_core::AiTool::Tagging,
            files: &FILES,
        };
        let models = tempfile::tempdir().unwrap();
        let dir = component_dir(models.path(), &component);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("good.onnx"), BODY).unwrap();
        std::fs::write(dir.join("bad.onnx"), BODY).unwrap();
        std::fs::write(dir.join(MARKER), b"").unwrap();
        assert!(is_installed(models.path(), &component));
        assert!(!remove_damaged(models.path(), &component).await.unwrap());
        assert!(is_installed(models.path(), &component));

        // Same size, other bytes: only a hash tells.
        let mut changed = BODY.to_vec();
        changed[0] ^= 1;
        std::fs::write(dir.join("bad.onnx"), changed).unwrap();
        assert!(is_installed(models.path(), &component));
        assert!(remove_damaged(models.path(), &component).await.unwrap());
        assert!(!is_installed(models.path(), &component));
        assert!(dir.join("good.onnx").is_file());
        assert!(!dir.join("bad.onnx").exists());
    }

    #[tokio::test]
    async fn an_unreachable_server_is_an_error_not_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let (dest, part) = (dir.path().join("f"), dir.path().join("f.part"));
        let err = download("http://127.0.0.1:9/f", &dest, &part, "00", 4, |_| {}).await;
        assert!(err.is_err());
    }

    /// A ZIP of `(name, method, contents)`, the way a NuGet package is laid out.
    fn zip(files: &[(&str, u16, &[u8])]) -> Vec<u8> {
        use std::io::Write;
        let (mut out, mut directory) = (Vec::new(), Vec::new());
        for (name, method, contents) in files {
            let packed = if *method == 8 {
                let mut z = flate2::write::DeflateEncoder::new(Vec::new(), Default::default());
                z.write_all(contents).unwrap();
                z.finish().unwrap()
            } else {
                contents.to_vec()
            };
            let offset = out.len() as u32;
            let sizes = [packed.len() as u32, contents.len() as u32];
            out.extend(0x0403_4b50u32.to_le_bytes());
            out.extend([20, 0, 0, 0]);
            out.extend(method.to_le_bytes());
            out.extend([0; 8]); // time, date, crc
            out.extend(sizes.iter().flat_map(|s| s.to_le_bytes()));
            out.extend((name.len() as u16).to_le_bytes());
            out.extend(3u16.to_le_bytes());
            out.extend(name.as_bytes());
            out.extend(b"xyz");
            out.extend(&packed);
            directory.extend(0x0201_4b50u32.to_le_bytes());
            directory.extend([20, 0, 20, 0, 0, 0]);
            directory.extend(method.to_le_bytes());
            directory.extend([0; 8]);
            directory.extend(sizes.iter().flat_map(|s| s.to_le_bytes()));
            directory.extend((name.len() as u16).to_le_bytes());
            directory.extend([0; 12]);
            directory.extend(offset.to_le_bytes());
            directory.extend(name.as_bytes());
        }
        let at = out.len() as u32;
        let count = (files.len() as u16).to_le_bytes();
        out.extend(&directory);
        out.extend(0x0605_4b50u32.to_le_bytes());
        out.extend([0; 4]);
        out.extend(count);
        out.extend(count);
        out.extend((directory.len() as u32).to_le_bytes());
        out.extend(at.to_le_bytes());
        out.extend([0; 2]);
        out
    }

    #[test]
    fn takes_one_file_out_of_a_package() {
        let dir = tempfile::tempdir().unwrap();
        let (archive, dest) = (dir.path().join("p.nupkg"), dir.path().join("lib"));
        let library = BODY.repeat(50);
        std::fs::write(
            &archive,
            zip(&[
                ("runtimes/win-x64/native/onnxruntime.dll", 8, b"other"),
                ("runtimes/linux-x64/native/libonnxruntime.so", 8, &library),
                ("stored.txt", 0, b"as is"),
            ]),
        )
        .unwrap();
        let member = "runtimes/linux-x64/native/libonnxruntime.so";
        let size = library.len() as u64;
        assert!(unzip_file(&archive, member, size, &dest).unwrap());
        assert_eq!(std::fs::read(&dest).unwrap(), library);
        assert!(unzip_file(&archive, "stored.txt", 5, &dest).unwrap());
        assert_eq!(std::fs::read(&dest).unwrap(), b"as is");
        assert!(!unzip_file(&archive, "missing", 5, &dest).unwrap());
    }

    /// Regression: a member was inflated without a limit, so a few packed
    /// bytes from a ranged read could fill the disk before its hash failed.
    #[test]
    fn a_member_larger_than_pinned_is_cut_off() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("lib");
        let huge = vec![0u8; 4 << 20];
        let mut packed = flate2::write::DeflateEncoder::new(Vec::new(), Default::default());
        std::io::Write::write_all(&mut packed, &huge).unwrap();
        let packed = packed.finish().unwrap();
        assert!(packed.len() < 10_000);
        assert!(!unpack(8, packed.as_slice(), 1_000, &dest).unwrap());
        assert!(!dest.exists());
        assert!(!unpack(0, huge.as_slice(), 1_000, &dest).unwrap());
        assert!(!dest.exists());
        // The pinned size itself is fine.
        assert!(unpack(8, packed.as_slice(), huge.len() as u64, &dest).unwrap());
        assert_eq!(std::fs::metadata(&dest).unwrap().len(), huge.len() as u64);
    }

    /// A pin for `contents` at `path`, as the real ones are.
    fn pin(path: &'static str, contents: &[u8]) -> Member {
        Member {
            path,
            sha256: Box::leak(digest(contents).into_boxed_str()),
            size: contents.len() as u64,
        }
    }

    /// Serves `body` at `/p`, answering ranges when `ranges` is set, and
    /// counts the bytes it sent.
    async fn package_server(body: Vec<u8>, ranges: bool) -> (String, Arc<AtomicUsize>) {
        let sent = Arc::new(AtomicUsize::new(0));
        let counter = sent.clone();
        let body = Arc::new(body);
        let app = Router::new().route(
            "/p",
            get(move |headers: HeaderMap| {
                let (counter, body) = (counter.clone(), body.clone());
                async move {
                    let asked = headers
                        .get("range")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.strip_prefix("bytes="))
                        .and_then(|v| v.split_once('-'))
                        .and_then(|(a, b)| {
                            Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?))
                        });
                    let (status, out) = match asked.filter(|_| ranges) {
                        Some((a, b)) => (
                            StatusCode::PARTIAL_CONTENT,
                            body.get(a..=b.min(body.len() - 1))
                                .unwrap_or_default()
                                .to_vec(),
                        ),
                        None => (StatusCode::OK, body.to_vec()),
                    };
                    counter.fetch_add(out.len(), Ordering::SeqCst);
                    (status, out)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        (format!("http://{addr}/p"), sent)
    }

    #[tokio::test]
    async fn fetches_only_the_library_when_it_can() {
        let library = BODY.repeat(500);
        // Most of a package is other platforms' libraries.
        let other: Vec<u8> = (0..400_000u32).map(|i| (i * 7919 % 251) as u8).collect();
        let member = "runtimes/linux-x64/native/libonnxruntime.so";
        let package = zip(&[
            ("runtimes/win-x64/native/onnxruntime.dll", 0, &other),
            (member, 8, &library),
        ]);
        for ranges in [true, false] {
            let (url, sent) = package_server(package.clone(), ranges).await;
            let dir = tempfile::tempdir().unwrap();
            let dest = dir.path().join("runtime").join("lib.so");
            let pinned = Package {
                url,
                sha256: digest(&package),
                size: package.len() as u64,
            };
            fetch_runtime(&pinned, &pin(member, &library), &dest)
                .await
                .unwrap();
            assert_eq!(std::fs::read(&dest).unwrap(), library);
            let sent = sent.load(Ordering::SeqCst);
            if ranges {
                assert!(sent < 100_000, "{sent} of {}", package.len());
            } else {
                assert!(sent >= package.len(), "{sent}");
            }
            // Nothing is left behind.
            let left: Vec<_> = std::fs::read_dir(dir.path().join("runtime"))
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(left, ["lib.so"]);
            // A library that isn't the pinned one is refused, either way.
            let wrong = fetch_runtime(&pinned, &pin(member, b"other"), &dest).await;
            assert!(wrong.is_err());
            // So is one larger than its pin, and none of it is left.
            let short = Member {
                size: library.len() as u64 - 1,
                ..pin(member, &library)
            };
            assert!(fetch_runtime(&pinned, &short, &dest).await.is_err());
            let left = std::fs::read_dir(dir.path().join("runtime"))
                .unwrap()
                .count();
            assert_eq!(left, 1);
        }
    }

    #[test]
    fn a_damaged_package_is_no_crash() {
        let dir = tempfile::tempdir().unwrap();
        let (archive, dest) = (dir.path().join("p.nupkg"), dir.path().join("lib"));
        let good = zip(&[("a", 8, b"contents")]);
        for cut in [0, 10, good.len() / 2, good.len() - 30, good.len() - 1] {
            let mut bad = good.clone();
            bad.truncate(cut);
            std::fs::write(&archive, &bad).unwrap();
            let _ = unzip_file(&archive, "a", 8, &dest);
        }
        // Offsets pointing anywhere.
        for flip in 0..good.len() {
            let mut bad = good.clone();
            bad[flip] ^= 0xff;
            std::fs::write(&archive, &bad).unwrap();
            let _ = unzip_file(&archive, "a", 8, &dest);
        }
    }

    /// The pinned runtime, from NuGet. Skipped unless `STARBOORU_TEST_NUGET`
    /// is set, as it goes online.
    #[tokio::test]
    async fn the_pinned_runtime_comes_from_nuget() {
        if std::env::var_os("STARBOORU_TEST_NUGET").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("runtime").join("libonnxruntime.so");
        install_runtime(&library).await.unwrap();
        let pin = RUNTIMES
            .iter()
            .find(|r| r.arch == std::env::consts::ARCH)
            .unwrap();
        assert_eq!(
            sha256_of(library.clone()).await.unwrap(),
            pin.library.sha256
        );
        assert_eq!(std::fs::metadata(&library).unwrap().len(), pin.library.size);
    }
}
