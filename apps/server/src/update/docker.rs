//! A small client for the Docker Engine API on its Unix socket: HTTP/1.1
//! through hyper, one connection per request. Paths carry no API version,
//! so the daemon answers in its own, which every version since 1.24 reads.

use std::path::{Path, PathBuf};
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;

/// Answers the client reads whole; an inspect is a few kilobytes.
const MAX_ANSWER_BYTES: usize = 16 << 20;
/// One line of a pull's progress.
const MAX_LINE_BYTES: usize = 64 << 10;
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// A big image on a slow line.
const PULL_TIMEOUT: Duration = Duration::from_secs(2 * 3600);

/// Where the Docker socket usually is.
pub const DEFAULT_SOCKET: &str = "/var/run/docker.sock";

#[derive(Debug, Clone)]
pub struct Docker {
    socket: PathBuf,
}

impl Docker {
    /// The socket `DOCKER_HOST` names (`unix://` only), or the usual one.
    /// `Ok(None)` when there is none; `Err` for a `DOCKER_HOST` this client
    /// can't use.
    pub fn find(docker_host: Option<&str>) -> Result<Option<Self>, String> {
        let socket = match docker_host.map(str::trim).filter(|h| !h.is_empty()) {
            Some(host) => match host.strip_prefix("unix://") {
                Some(path) if path.starts_with('/') => PathBuf::from(path),
                _ => {
                    return Err(format!(
                        "DOCKER_HOST={host}: only a Unix socket (unix:///…) is supported"
                    ));
                }
            },
            None => PathBuf::from(DEFAULT_SOCKET),
        };
        Ok(socket.exists().then_some(Self { socket }))
    }

    #[cfg(test)]
    pub fn at(socket: PathBuf) -> Self {
        Self { socket }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<hyper::Response<hyper::body::Incoming>, String> {
        let stream = tokio::net::UnixStream::connect(&self.socket)
            .await
            .map_err(|e| format!("can't reach Docker at {}: {e}", self.socket.display()))?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| format!("Docker didn't answer: {e}"))?;
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::debug!("Docker connection: {e}");
            }
        });
        let bytes = match body {
            Some(body) => Bytes::from(serde_json::to_vec(body).map_err(|e| e.to_string())?),
            None => Bytes::new(),
        };
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header(hyper::header::HOST, "docker");
        if body.is_some() {
            req = req.header(hyper::header::CONTENT_TYPE, "application/json");
        }
        let req = req.body(Full::new(bytes)).map_err(|e| e.to_string())?;
        sender
            .send_request(req)
            .await
            .map_err(|e| format!("Docker didn't answer: {e}"))
    }

    /// Sends a request and reads the whole answer, which must be a success.
    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, String> {
        let work = async {
            let resp = self.send(method, path, body).await?;
            let status = resp.status();
            let mut incoming = resp.into_body();
            let mut answer = Vec::new();
            while let Some(frame) = incoming.frame().await {
                let frame = frame.map_err(|e| format!("Docker's answer was cut short: {e}"))?;
                if let Some(data) = frame.data_ref() {
                    if answer.len().saturating_add(data.len()) > MAX_ANSWER_BYTES {
                        return Err("Docker's answer is too large".to_owned());
                    }
                    answer.extend_from_slice(data);
                }
            }
            Ok((status, answer))
        };
        let (status, answer) = tokio::time::timeout(CALL_TIMEOUT, work)
            .await
            .map_err(|_| "Docker didn't answer in time".to_owned())??;
        if !status.is_success() && status != StatusCode::NOT_MODIFIED {
            return Err(error_of(status, &answer));
        }
        if answer.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&answer).map_err(|_| "Docker's answer can't be read".to_owned())
    }

    pub async fn get(&self, path: &str) -> Result<Value, String> {
        self.call(Method::GET, path, None).await
    }

    pub async fn post(&self, path: &str, body: Option<&Value>) -> Result<Value, String> {
        self.call(Method::POST, path, body).await
    }

    pub async fn delete(&self, path: &str) -> Result<(), String> {
        self.call(Method::DELETE, path, None).await.map(|_| ())
    }

    /// `GET /containers/{id}/json`.
    pub async fn container(&self, id: &str) -> Result<Value, String> {
        self.get(&format!("/containers/{}/json", segment(id))).await
    }

    /// `GET /images/{id}/json`.
    pub async fn image(&self, id: &str) -> Result<Value, String> {
        self.get(&format!("/images/{}/json", segment(id))).await
    }

    /// Pulls `repo:tag` and waits until it is done. Docker reports a failed
    /// pull inside a successful answer, as the last line of its progress.
    pub async fn pull(&self, repo: &str, tag: &str) -> Result<(), String> {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("fromImage", repo)
            .append_pair("tag", tag)
            .finish();
        let work = async {
            let resp = self
                .send(Method::POST, &format!("/images/create?{query}"), None)
                .await?;
            let status = resp.status();
            let mut incoming = resp.into_body();
            let mut lines = Lines::default();
            while let Some(frame) = incoming.frame().await {
                let frame = frame.map_err(|e| format!("the download was cut short: {e}"))?;
                if let Some(data) = frame.data_ref() {
                    lines.push(data);
                }
            }
            lines.finish();
            if !status.is_success() {
                return Err(error_of(status, &lines.first));
            }
            match lines.error {
                Some(error) => Err(error),
                None => Ok(()),
            }
        };
        tokio::time::timeout(PULL_TIMEOUT, work)
            .await
            .map_err(|_| "the image took too long to download".to_owned())?
    }
}

/// A pull's progress, one JSON object a line; only an error is kept.
#[derive(Default)]
struct Lines {
    line: Vec<u8>,
    /// The start of the answer, for an error that isn't in lines.
    first: Vec<u8>,
    error: Option<String>,
}

impl Lines {
    fn push(&mut self, data: &[u8]) {
        if self.first.len() < MAX_LINE_BYTES {
            let room = MAX_LINE_BYTES.saturating_sub(self.first.len());
            self.first
                .extend_from_slice(data.get(..room.min(data.len())).unwrap_or_default());
        }
        for &b in data {
            if b == b'\n' {
                self.end_line();
            } else if self.line.len() < MAX_LINE_BYTES {
                self.line.push(b);
            }
        }
    }

    fn end_line(&mut self) {
        if let Ok(value) = serde_json::from_slice::<Value>(&self.line)
            && let Some(error) = value
                .pointer("/errorDetail/message")
                .or_else(|| value.get("error"))
                .and_then(Value::as_str)
        {
            self.error = Some(error.chars().take(500).collect());
        }
        self.line.clear();
    }

    fn finish(&mut self) {
        if !self.line.is_empty() {
            self.end_line();
        }
    }
}

/// Docker's `{"message": …}`, or the status.
fn error_of(status: StatusCode, body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_owned))
        .map(|m| m.trim().chars().take(500).collect())
        .unwrap_or_else(|| format!("Docker answered {}", status.as_u16()))
}

/// One path segment: container names and ids are plain, image references
/// may hold `/` and `:`.
pub fn segment(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_socket() {
        assert!(Docker::find(Some("tcp://127.0.0.1:2375")).is_err());
        assert!(Docker::find(Some("unix://relative.sock")).is_err());
        assert!(
            Docker::find(Some("unix:///nowhere/docker.sock"))
                .unwrap()
                .is_none()
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docker.sock");
        std::fs::write(&path, b"").unwrap();
        let found = Docker::find(Some(&format!("unix://{}", path.display())))
            .unwrap()
            .unwrap();
        assert_eq!(found.socket(), path);
    }

    #[test]
    fn a_pull_reports_its_last_error() {
        let mut lines = Lines::default();
        lines.push(b"{\"status\":\"Pulling from starbooru-server\",\"id\":\"0.3.0\"}\r\n{\"status\":\"Down");
        lines.push(b"loading\",\"progressDetail\":{\"current\":1}}\r\n");
        lines.finish();
        assert_eq!(lines.error, None);
        lines.push(
            b"{\"errorDetail\":{\"message\":\"manifest unknown\"},\"error\":\"manifest unknown\"}",
        );
        lines.finish();
        assert_eq!(lines.error.as_deref(), Some("manifest unknown"));
        assert_eq!(
            error_of(
                StatusCode::NOT_FOUND,
                br#"{"message":"No such container: x"}"#
            ),
            "No such container: x"
        );
        assert_eq!(
            error_of(StatusCode::BAD_GATEWAY, b"<html>"),
            "Docker answered 502"
        );
    }

    #[test]
    fn segments_are_encoded() {
        assert_eq!(segment("abc123"), "abc123");
        assert_eq!(
            segment("localhost:5110/starbooru-server:0.3.0"),
            "localhost%3A5110%2Fstarbooru-server%3A0.3.0"
        );
    }

    /// A Docker that answers on a socket, for the client itself.
    #[tokio::test]
    async fn talks_http_over_the_socket() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docker.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let json = |status: &str, body: &str| {
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
        };
        let chunk = |line: &str| format!("{:x}\r\n{line}\r\n", line.len());
        let answers = [
            json("200 OK", r#"{"Id":"abc","Name":"/x"}"#),
            json("404 Not Found", r#"{"message":"No such container: y"}"#),
            "HTTP/1.1 204 No Content\r\n\r\n".to_owned(),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n{}{}0\r\n\r\n",
                chunk("{\"status\":\"Pulling fs layer\"}\r\n"),
                chunk("{\"error\":\"pull access denied for starbooru\"}\r\n"),
            ),
        ];
        let server = tokio::spawn(async move {
            let mut seen = Vec::new();
            for answer in answers {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap();
                seen.push(String::from_utf8_lossy(&buf[..n]).into_owned());
                sock.write_all(answer.as_bytes()).await.unwrap();
                sock.shutdown().await.unwrap();
            }
            seen
        });
        let docker = Docker::at(path);
        let found = docker.container("abc").await.unwrap();
        assert_eq!(found["Name"], "/x");
        assert_eq!(
            docker.container("y").await.unwrap_err(),
            "No such container: y"
        );
        docker.post("/containers/abc/start", None).await.unwrap();
        assert_eq!(
            docker.pull("localhost:5110/starbooru", "0.3.0").await,
            Err("pull access denied for starbooru".to_owned())
        );
        let seen = server.await.unwrap();
        assert!(seen[0].starts_with("GET /containers/abc/json HTTP/1.1\r\n"));
        assert!(seen[2].starts_with("POST /containers/abc/start HTTP/1.1\r\n"));
        assert!(seen[3].starts_with(
            "POST /images/create?fromImage=localhost%3A5110%2Fstarbooru&tag=0.3.0 HTTP/1.1\r\n"
        ));
    }
}
