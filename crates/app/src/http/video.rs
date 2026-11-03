//! Video-only transport: metadata discovery, bounded preview ranges and eager
//! full downloads on Play. Image request budgets and buffering stay intact.
use super::{authenticate, http, needs_github_token, TokenProvider, UreqHttpClient};
use anyhow::ensure;
#[path = "video/progressive.rs"]
mod progressive;
use futures::channel::oneshot;
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    time::Duration,
};

pub(crate) const MAX_VIDEO_BYTES: u64 = 128 * 1024 * 1024;
#[derive(Default)]
pub(crate) struct Transfer {
    pub bytes: AtomicU64,
    pub total: AtomicU64,
    pub canceled: AtomicBool,
    pub started: AtomicBool,
    pub complete: AtomicBool,
    pub error: Mutex<Option<String>>,
    event: Mutex<()>,
    changed: Condvar,
}
impl Transfer {
    pub fn cancel(&self) {
        self.canceled.store(true, Ordering::Relaxed);
        self.wake();
    }
    fn wake(&self) {
        let _event = self.event.lock().unwrap();
        self.changed.notify_all();
    }
    fn check(&self) -> anyhow::Result<()> {
        ensure!(!self.canceled.load(Ordering::Relaxed), "Download canceled");
        Ok(())
    }
}

pub(crate) struct VideoFile {
    pub path: PathBuf,
    // Cache owns a 0700 directory / 0600 sparse file. Only complete ranges are
    // exposed to the decoder; no signed URLs or credentials are written.
    progressive: Arc<progressive::Cache>,
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .redirects(10)
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(5))
        .timeout(Duration::from_secs(10 * 60))
        .try_proxy_from_env(true)
        .redirect_auth_headers(ureq::RedirectAuthHeaders::Never)
        .build()
}

pub(crate) fn safe_url(url: &str) -> bool {
    http_client::Url::parse(url).is_ok_and(|url| {
        matches!(url.scheme(), "https" | "http")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
    })
}

pub(crate) fn direct_video(url: &str) -> bool {
    safe_url(url)
        && http_client::Url::parse(url).is_ok_and(|url| {
            Path::new(url.path())
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|ext| {
                    ["mp4", "m4v", "mov", "webm"]
                        .iter()
                        .any(|e| ext.eq_ignore_ascii_case(e))
                })
        })
}

pub(crate) fn attachment(url: &str) -> bool {
    // Discover only GitHub attachments, never arbitrary raw files/hosts.
    // CDN discovery is unauthenticated; the credential allowlist is unchanged.
    safe_url(url)
        && http_client::Url::parse(url).is_ok_and(|parsed| {
            if parsed.scheme() != "https" || parsed.port_or_known_default() != Some(443) {
                return false;
            }
            match parsed.host_str() {
                Some("github.com") => needs_github_token(url) && !parsed.path().contains("/raw/"),
                Some(
                    "user-images.githubusercontent.com"
                    | "private-user-images.githubusercontent.com",
                ) => true,
                _ => false,
            }
        })
}

pub(crate) fn is_video_type(value: &str) -> bool {
    value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .starts_with("video/")
}

pub(crate) fn discover(url: String) -> oneshot::Receiver<anyhow::Result<bool>> {
    let (tx, rx) = oneshot::channel();
    std::thread::spawn(move || {
        let result = (|| {
            ensure!(attachment(&url), "Not a GitHub attachment");
            let client = UreqHttpClient::new();
            probe(&url, &client.token)
        })();
        let _ = tx.send(result);
    });
    rx
}

fn probe(url: &str, token: &TokenProvider) -> anyhow::Result<bool> {
    // GitHub redirects uploads to GET-signed S3 URLs: following them with HEAD
    // fails signature validation (403), even when GitHub authentication worked.
    // Ask for one byte instead, but inspect headers only and drop the response.
    // Never read/cache a media body here, even if a server ignores Range.
    let response = authenticate(
        agent()
            .get(url)
            .set("Range", "bytes=0-0")
            .set("Accept-Encoding", "identity")
            .timeout(Duration::from_secs(30)),
        url,
        &http::HeaderMap::new(),
        token,
    )
    .call()?;
    Ok(is_video_type(
        response.header("Content-Type").unwrap_or_default(),
    ))
}

/// Prepare a progressive source after its first bounded range. This does not
/// wait for the full file; `VideoFile::start` begins the eager download on Play.
pub(crate) fn download(
    url: String,
    transfer: Arc<Transfer>,
) -> oneshot::Receiver<anyhow::Result<VideoFile>> {
    let (tx, rx) = oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(progressive::prepare(url, transfer));
    });
    rx
}

impl VideoFile {
    pub fn source(&self) -> &str {
        self.progressive.endpoint()
    }
    pub fn start(&self) {
        self.progressive.start();
    }
}
impl Drop for VideoFile {
    fn drop(&mut self) {
        self.progressive.cancel();
    }
}

fn cache_extension(content_type: &str, url: &str) -> &'static str {
    match content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "video/webm" => "webm",
        "video/x-m4v" => "m4v",
        _ => http_client::Url::parse(url)
            .ok()
            .and_then(|url| {
                let ext = Path::new(url.path()).extension()?.to_str()?;
                ["mp4", "mov", "webm", "m4v"]
                    .into_iter()
                    .find(|known| ext.eq_ignore_ascii_case(known))
            })
            .unwrap_or("video"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_is_narrow_and_uses_mime_for_extensionless_attachments() {
        assert!(attachment("https://github.com/user-attachments/assets/id"));
        assert!(attachment("https://github.com/owner/repo/assets/123/id"));
        assert!(attachment(
            "https://private-user-images.githubusercontent.com/123/id?jwt=temporary"
        ));
        for url in [
            "https://example.com/file",
            "https://github.com/o/r/raw/main/a.mp4",
            "file:///a.mp4",
            "https://user:secret@github.com/user-attachments/assets/a",
        ] {
            assert!(!attachment(url));
        }
        assert!(direct_video("https://example.com/a.MOV?download=1"));
        assert!(!direct_video("https://example.com/a.mp4.exe"));
        assert!(!direct_video("file:///a.mp4"));
        assert!(is_video_type("Video/mp4; charset=binary"));
        assert!(!is_video_type("image/png"));
        assert_eq!(
            cache_extension("video/mp4", "https://github.com/user-attachments/assets/id"),
            "mp4"
        );
        assert_eq!(
            cache_extension("video/quicktime; charset=binary", "https://example.com/a"),
            "mov"
        );
        assert_eq!(
            cache_extension("application/octet-stream", "https://example.com/a.MOV?x=1"),
            "mov"
        );
    }
    fn server(response: String) -> (String, std::thread::JoinHandle<String>) {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut bytes = [0; 4096];
            let n = socket.read(&mut bytes).unwrap();
            socket.write_all(response.as_bytes()).unwrap();
            String::from_utf8_lossy(&bytes[..n]).into_owned()
        });
        (url, worker)
    }

    #[test]
    fn metadata_uses_a_one_byte_range_without_reading_bodies_or_guessing_types() {
        for (mime, expected) in [
            ("video/mp4", true),
            ("image/png", false),
            ("application/octet-stream", false),
            ("text/html", false),
        ] {
            let (url, worker) = server(format!("HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: 100000000\r\nConnection: close\r\n\r\n"));
            let token: TokenProvider = Arc::new(|| None);
            assert_eq!(probe(&url, &token).unwrap(), expected);
            let request = worker.join().unwrap().to_ascii_lowercase();
            assert!(request.starts_with("get / "));
            assert!(request.contains("range: bytes=0-0\r\n"));
            assert!(request.contains("accept-encoding: identity\r\n"));
        }
    }

    #[test]
    fn metadata_follows_get_signed_redirects_that_reject_head() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let cdn = format!("http://{}/signed-video", listener.local_addr().unwrap());
        let cdn_worker = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().unwrap();
                let mut bytes = [0; 4096];
                let n = socket.read(&mut bytes).unwrap();
                let request = String::from_utf8_lossy(&bytes[..n]).to_ascii_lowercase();
                if request.starts_with("get ") && request.contains("range: bytes=0-0\r\n") {
                    // No body: discovery must complete from headers alone.
                    socket.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Type: video/quicktime\r\nContent-Range: bytes 0-0/16145979\r\nContent-Length: 1\r\nConnection: close\r\n\r\n").unwrap();
                } else {
                    socket.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                }
                assert!(!request.contains("authorization:"));
            }
        });
        assert!(matches!(
            agent().head(&cdn).call(),
            Err(ureq::Error::Status(403, _))
        ));
        let (origin, origin_worker) = server(format!("HTTP/1.1 302 Found\r\nLocation: {cdn}?signed=not-persisted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"));
        let token: TokenProvider = Arc::new(|| None);
        assert!(probe(&origin, &token).unwrap());
        let request = origin_worker.join().unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get / "));
        assert!(request.contains("range: bytes=0-0\r\n"));
        cdn_worker.join().unwrap();
    }

    #[test]
    fn video_agent_never_forwards_authentication_to_redirects() {
        let (cdn, cdn_worker) = server("HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Length: 5\r\nConnection: close\r\n\r\nvideo".into());
        let (origin, origin_worker) = server(format!("HTTP/1.1 302 Found\r\nLocation: {cdn}?signed=not-persisted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"));
        let response = agent()
            .get(&origin)
            .set("Authorization", "token test-secret")
            .call()
            .unwrap();
        let mut bytes = Vec::new();
        response.into_reader().read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"video");
        assert!(origin_worker
            .join()
            .unwrap()
            .to_lowercase()
            .contains("authorization: token test-secret"));
        assert!(!cdn_worker
            .join()
            .unwrap()
            .to_lowercase()
            .contains("authorization:"));
    }

    #[test]
    fn html_errors_and_oversized_headers_do_not_become_playable_files() {
        for (mime, size) in [("text/html", 5), ("video/mp4", MAX_VIDEO_BYTES + 1)] {
            let (url, worker) = server(format!("HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n"));
            assert!(futures::executor::block_on(download(
                format!("{url}/clip.mp4"),
                Arc::default()
            ))
            .unwrap()
            .is_err());
            worker.join().unwrap();
        }
    }

    #[test]
    fn preview_rejects_empty_truncated_and_invalid_range_responses() {
        for response in [
            "HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Length: 20\r\nConnection: close\r\n\r\nshort",
            "HTTP/1.1 206 Partial Content\r\nContent-Type: video/mp4\r\nContent-Range: bytes 50-54/100\r\nContent-Length: 5\r\nConnection: close\r\n\r\nvideo",
        ] {
            let (url, worker) = server(response.into());
            assert!(futures::executor::block_on(download(url, Arc::default())).unwrap().is_err());
            worker.join().unwrap();
        }
    }

    #[test]
    fn cache_is_private_and_session_scoped() {
        use std::{net::TcpListener, os::unix::fs::PermissionsExt};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/a.mp4", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0; 2048];
            let _ = socket.read(&mut request);
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Length: 5\r\nConnection: close\r\n\r\nvideo").unwrap();
        });
        let file = futures::executor::block_on(download(url, Arc::default()))
            .unwrap()
            .unwrap();
        assert_eq!(std::fs::read(&file.path).unwrap(), b"video");
        assert_eq!(
            std::fs::metadata(&file.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let path = file.path.clone();
        drop(file);
        assert!(!path.exists());
    }
}
