//! Video-only requests: HEAD discovery, then opt-in streaming to a protected
//! session cache. Image request budgets and buffering are intentionally intact.
use super::{authenticate, http, needs_github_token, UreqHttpClient};
use anyhow::{bail, ensure, Context as _};
use futures::channel::oneshot;
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

pub(crate) const MAX_VIDEO_BYTES: u64 = 128 * 1024 * 1024;
// A single active session, at most one completed file and one partial file.
// Serializing workers prevents canceled/stalled requests accumulating on disk.
static DOWNLOAD_LOCK: Mutex<()> = Mutex::new(());

#[derive(Default)]
pub(crate) struct Transfer {
    pub bytes: AtomicU64,
    pub total: AtomicU64,
    pub canceled: AtomicBool,
}
impl Transfer {
    pub fn cancel(&self) {
        self.canceled.store(true, Ordering::Relaxed);
    }
    fn check(&self) -> anyhow::Result<()> {
        ensure!(!self.canceled.load(Ordering::Relaxed), "Download canceled");
        Ok(())
    }
}

pub(crate) struct VideoFile {
    pub path: PathBuf,
    // TempDir and NamedTempFile use 0700 / 0600; no response headers, signed
    // redirect URLs, cookies or tokens are ever written. Drop deletes the file.
    _directory: tempfile::TempDir,
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
            probe(authenticate(
                agent()
                    .head(&url)
                    .set("Accept-Encoding", "identity")
                    .timeout(Duration::from_secs(30)),
                &url,
                &http::HeaderMap::new(),
                &client.token,
            ))
        })();
        let _ = tx.send(result);
    });
    rx
}

fn probe(request: ureq::Request) -> anyhow::Result<bool> {
    let response = request.call()?;
    Ok(is_video_type(
        response.header("Content-Type").unwrap_or_default(),
    ))
}

pub(crate) fn download(
    url: String,
    transfer: Arc<Transfer>,
) -> oneshot::Receiver<anyhow::Result<VideoFile>> {
    let (tx, rx) = oneshot::channel();
    std::thread::spawn(move || {
        let result = (|| {
            let _guard = DOWNLOAD_LOCK.lock().unwrap();
            transfer.check()?;
            ensure!(safe_url(&url), "Only HTTP(S) video sources are supported");
            let client = UreqHttpClient::new();
            let response = authenticate(
                agent().get(&url).set("Accept-Encoding", "identity"),
                &url,
                &http::HeaderMap::new(),
                &client.token,
            )
            .call()
            .map_err(|_| anyhow::anyhow!("Could not download video"))?;
            // Keep diagnostics free of redirect URLs, cookies and tokens.
            let content_type = response.header("Content-Type").unwrap_or_default();
            ensure!(
                is_video_type(content_type)
                    || (direct_video(&url)
                        && content_type.split(';').next().unwrap_or_default().trim()
                            == "application/octet-stream"),
                "Unsupported video response (the server may require sign-in)"
            );
            let extension = cache_extension(content_type, &url);
            let total = response
                .header("Content-Length")
                .and_then(|s| s.parse::<u64>().ok());
            let directory = tempfile::Builder::new().prefix("gander-video-").tempdir()?;
            let mut partial = tempfile::NamedTempFile::new_in(directory.path())?;
            stream(
                response.into_reader(),
                partial.as_file_mut(),
                &transfer,
                total,
                MAX_VIDEO_BYTES,
            )?;
            partial.as_file().sync_all()?;
            // Completion becomes visible atomically, never a partially written
            // file. AVFoundation also needs a container suffix for local files;
            // extensionless GitHub URLs get one from response metadata.
            let path = directory.path().join(format!("video.{extension}"));
            partial
                .persist(&path)
                .context("Could not complete video cache file")?;
            Ok(VideoFile {
                path,
                _directory: directory,
            })
        })();
        let _ = tx.send(result);
    });
    rx
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

fn stream(
    mut reader: impl Read,
    mut file: impl Write,
    transfer: &Transfer,
    total: Option<u64>,
    limit: u64,
) -> anyhow::Result<()> {
    ensure!(
        total.is_none_or(|n| n <= limit),
        "Video exceeds 128 MiB download limit"
    );
    transfer.total.store(total.unwrap_or(0), Ordering::Relaxed);
    let mut bytes = 0;
    let mut buffer = [0; 64 * 1024];
    loop {
        transfer.check()?;
        let n = reader.read(&mut buffer)?;
        transfer.check()?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        if bytes > limit {
            bail!("Video exceeds 128 MiB download limit");
        }
        file.write_all(&buffer[..n])?;
        transfer.bytes.store(bytes, Ordering::Relaxed);
    }
    ensure!(bytes > 0, "Empty video response");
    ensure!(
        total.is_none_or(|n| bytes == n),
        "Incomplete video response"
    );
    Ok(())
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
    #[test]
    fn streaming_bounds_progress_cancellation_and_short_bodies() {
        let transfer = Transfer::default();
        let mut out = Vec::new();
        stream(&b"video"[..], &mut out, &transfer, Some(5), 8).unwrap();
        assert_eq!(out, b"video");
        assert_eq!(transfer.bytes.load(Ordering::Relaxed), 5);
        assert!(stream(&b"too big"[..], Vec::new(), &transfer, None, 3).is_err());
        assert!(stream(&b"short"[..], Vec::new(), &transfer, Some(6), 8).is_err());
        assert!(stream(&b""[..], Vec::new(), &transfer, None, 8).is_err());
        transfer.cancel();
        assert!(stream(&b"video"[..], Vec::new(), &transfer, None, 8).is_err());
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
    fn metadata_is_head_only_and_does_not_guess_extensionless_types() {
        for (mime, expected) in [
            ("video/mp4", true),
            ("image/png", false),
            ("application/octet-stream", false),
            ("text/html", false),
        ] {
            let (url, worker) = server(format!("HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: 100000000\r\nConnection: close\r\n\r\n"));
            assert_eq!(probe(agent().head(&url)).unwrap(), expected);
            assert!(worker.join().unwrap().starts_with("HEAD / "));
        }
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
    fn cancellation_during_read_does_not_write_the_next_chunk() {
        struct CancelReader<'a>(&'a Transfer);
        impl Read for CancelReader<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.0.cancel();
                buffer[0] = 1;
                Ok(1)
            }
        }
        let transfer = Transfer::default();
        let mut written = Vec::new();
        assert!(stream(CancelReader(&transfer), &mut written, &transfer, None, 8).is_err());
        assert!(written.is_empty());
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
    fn download_is_private_atomic_and_session_scoped() {
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
