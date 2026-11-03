//! The app's HTTP client, registered with gpui at startup via
//! `with_http_client`. gpui itself ships a `NullHttpClient` — without this
//! module, every remote `img()` (account avatars and conversation media)
//! silently fails to load and renders as the Avatar's empty circle.
//!
//! Implementation: ureq — blocking, rustls, a tiny dependency tree (no
//! tokio/hyper) — run on a short-lived thread per request and bridged into
//! gpui's async world through a oneshot channel. v0.1's traffic is small
//! image fetches, where a thread per request beats a resident runtime.

pub(crate) mod video;

use futures::channel::oneshot;
use futures::future::BoxFuture;
use futures::{AsyncReadExt as _, FutureExt as _};
use http_client::{http, AsyncBody, HttpClient, RedirectPolicy, Url};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Read as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::Duration;

/// Time budget for one request, connect and response combined. Image
/// fetches are small; anything longer is a stuck connection not worth
/// holding a thread for.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_IMAGE_BYTES: u64 = 25 * 1024 * 1024;

type TokenProvider = Arc<dyn Fn() -> Option<String> + Send + Sync>;
type RequestLocks = Arc<Mutex<HashMap<String, Weak<Mutex<()>>>>>;

/// The client the app registers with gpui in `main`.
pub fn http_client() -> Arc<dyn HttpClient> {
    Arc::new(UreqHttpClient::new())
}

struct UreqHttpClient {
    /// Redirects followed — the path gpui's image loader uses
    /// (`get(…, follow_redirects: true)`).
    follow: ureq::Agent,
    /// Redirects refused (`RedirectPolicy::NoFollow`, the trait default).
    no_follow: ureq::Agent,
    token: TokenProvider,
    cache_dir: Option<PathBuf>,
    // Coalesce image-loader/metadata requests, including simultaneous instances
    // of the same attachment. No lock is ever held on the UI thread.
    cache_locks: RequestLocks,
}

impl UreqHttpClient {
    fn new() -> Self {
        // Both agents honor HTTP(S)_PROXY from the environment, the same
        // way the `git`/`gh` subprocesses the app runs already do.
        fn agent(redirects: u32) -> ureq::Agent {
            ureq::AgentBuilder::new()
                .redirects(redirects)
                .timeout(REQUEST_TIMEOUT)
                .try_proxy_from_env(true)
                // Signed CDN redirects must never receive the GitHub token.
                .redirect_auth_headers(ureq::RedirectAuthHeaders::Never)
                .build()
        }
        Self {
            follow: agent(10),
            no_follow: agent(0),
            token: Arc::new(|| {
                let output = std::process::Command::new("gh")
                    .args(["auth", "token", "--hostname", "github.com"])
                    .stdin(std::process::Stdio::null())
                    .output()
                    .ok()?;
                if !output.status.success() {
                    return None;
                }
                let token = String::from_utf8(output.stdout).ok()?.trim().to_owned();
                (!token.is_empty()).then_some(token)
            }),
            cache_dir: std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".cache/gander/images")),
            cache_locks: Default::default(),
        }
    }
}

impl HttpClient for UreqHttpClient {
    fn user_agent(&self) -> Option<&http::HeaderValue> {
        None
    }

    fn proxy(&self) -> Option<&Url> {
        None
    }

    fn send(
        &self,
        req: http::Request<AsyncBody>,
    ) -> BoxFuture<'static, anyhow::Result<http::Response<AsyncBody>>> {
        // The request's redirect policy rides as a builder extension (see
        // `http_client::HttpRequestExt`); pick the matching agent before
        // leaving the async world.
        let agent = match req.extensions().get::<RedirectPolicy>() {
            Some(RedirectPolicy::FollowAll) | Some(RedirectPolicy::FollowLimit(_)) => {
                self.follow.clone()
            }
            Some(RedirectPolicy::NoFollow) | None => self.no_follow.clone(),
        };
        // Do not turn a later NoFollow request into the cached result of a
        // followed redirect. gpui image loads always opt into following.
        let cacheable = req.method() == http::Method::GET
            && matches!(
                req.extensions().get::<RedirectPolicy>(),
                Some(RedirectPolicy::FollowAll | RedirectPolicy::FollowLimit(_))
            );
        let token = self.token.clone();
        let cache_dir = self.cache_dir.clone();
        let cache_locks = self.cache_locks.clone();
        async move {
            // The body must be read off the request before it can cross to
            // a blocking thread. GETs carry `AsyncBody::empty`, which reads
            // zero bytes immediately.
            let (parts, mut body) = req.into_parts();
            let mut req_body = Vec::new();
            body.read_to_end(&mut req_body).await?;

            let (tx, rx) = oneshot::channel();
            thread::spawn(move || {
                let _ = tx.send((|| {
                    let cacheable = cacheable && req_body.is_empty();
                    let lock = cacheable.then(|| {
                        let mut locks = cache_locks.lock().unwrap();
                        locks.retain(|_, lock| lock.strong_count() > 0);
                        let entry = locks.entry(parts.uri.to_string()).or_default();
                        entry.upgrade().unwrap_or_else(|| {
                            let lock = Arc::new(Mutex::new(()));
                            *entry = Arc::downgrade(&lock);
                            lock
                        })
                    });
                    let _guard = lock.as_ref().map(|lock| lock.lock().unwrap());
                    let cache = cacheable.then_some(cache_dir).flatten().map(|dir| {
                        dir.join(format!(
                            "{:x}",
                            Sha256::digest(parts.uri.to_string().as_bytes())
                        ))
                    });
                    if let Some(response) = cache.as_deref().and_then(read_cached) {
                        return Ok(response);
                    }
                    let response = run(agent, parts, req_body, &token)?;
                    if let Some(cache) = cache {
                        // A cache failure is never a fetch failure.
                        let _ = write_cached(&cache, &response);
                    }
                    response.into_http()
                })());
            });
            rx.await
                .map_err(|_| anyhow::anyhow!("HTTP worker thread died"))?
        }
        .boxed()
    }
}

/// One blocking request/response round trip on the worker thread: gpui's
/// request in, gpui's response out. Non-2xx statuses are *responses*, not
/// errors — callers like gpui's image loader check `status().is_success()`
/// themselves, so they must be allowed to see the status.
fn run(
    agent: ureq::Agent,
    parts: http::request::Parts,
    body: Vec<u8>,
    token: &TokenProvider,
) -> anyhow::Result<BufferedResponse> {
    let url = parts.uri.to_string();
    let mut request = agent.request(parts.method.as_str(), &url);
    for (name, value) in parts.headers.iter() {
        // ureq speaks &str headers; a non-UTF-8 value has no way across,
        // so it's skipped rather than failing the whole request.
        if let Ok(value) = value.to_str() {
            request = request.set(name.as_str(), value);
        }
    }
    request = authenticate(request, &url, &parts.headers, token);
    let response = if body.is_empty() {
        request.call()
    } else {
        request.send_bytes(&body)
    };
    let response = match response {
        Ok(response) => response,
        // ureq folds 4xx/5xx into Err(Status(code, response)); unfold them
        // back into plain responses so callers see real statuses.
        Err(ureq::Error::Status(_, response)) => response,
        Err(ureq::Error::Transport(err)) => return Err(err.into()),
    };

    let mut headers = http::HeaderMap::new();
    for name in response.headers_names() {
        if let Some(value) = response.header(&name) {
            headers.insert(
                http::HeaderName::from_bytes(name.as_bytes())?,
                value.parse()?,
            );
        }
    }
    let status = response.status();
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_IMAGE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_IMAGE_BYTES,
        "Image exceeds 25 MiB"
    );
    Ok(BufferedResponse {
        status,
        headers,
        bytes,
    })
}

fn authenticate(
    request: ureq::Request,
    url: &str,
    headers: &http::HeaderMap,
    token: &TokenProvider,
) -> ureq::Request {
    if needs_github_token(url) && !headers.contains_key(http::header::AUTHORIZATION) {
        if let Some(token) = token() {
            return request.set("Authorization", &format!("token {token}"));
        }
    }
    request
}

/// Only HTTPS GitHub attachment/raw endpoints get credentials. Not arbitrary
/// comment URLs, lookalike hosts, avatars, or self-authenticating CDN URLs.
fn needs_github_token(url: &str) -> bool {
    let Ok(url) = Url::parse(url) else {
        return false;
    };
    if url.scheme() != "https"
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return false;
    }
    match url.host_str() {
        Some("raw.githubusercontent.com") => true,
        Some("github.com") => {
            let segments: Vec<_> = url.path().split('/').filter(|s| !s.is_empty()).collect();
            matches!(
                segments.as_slice(),
                ["user-attachments", "assets" | "files", _, ..]
            ) || matches!(segments.as_slice(), [_, _, "assets" | "raw", _, ..])
        }
        _ => false,
    }
}

struct BufferedResponse {
    status: u16,
    headers: http::HeaderMap,
    bytes: Vec<u8>,
}

impl BufferedResponse {
    fn into_http(self) -> anyhow::Result<http::Response<AsyncBody>> {
        let mut response = http::Response::builder()
            .status(self.status)
            .body(AsyncBody::from(self.bytes))?;
        *response.headers_mut() = self.headers;
        Ok(response)
    }
}

fn image_content_type(response: &BufferedResponse) -> Option<&str> {
    if response.status != 200 || response.bytes.is_empty() {
        return None;
    }
    let content_type = response
        .headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();
    if content_type.starts_with("image/") {
        return Some(content_type);
    }
    if is_svg(content_type, &response.bytes) {
        return Some("image/svg+xml");
    }
    image::guess_format(&response.bytes)
        .ok()
        .map(|format| format.to_mime_type())
}

pub(crate) fn is_svg(content_type: &str, bytes: &[u8]) -> bool {
    content_type
        .split(';')
        .next()
        .is_some_and(|s| s.trim().eq_ignore_ascii_case("image/svg+xml"))
        || std::str::from_utf8(bytes).is_ok_and(|text| {
            let mut text = text.trim_start_matches('\u{feff}').trim_start();
            loop {
                if let Some(rest) = text.strip_prefix("<!--") {
                    let Some(end) = rest.find("-->") else {
                        return false;
                    };
                    text = rest[end + 3..].trim_start();
                } else if let Some(rest) = text.strip_prefix("<?xml") {
                    let Some(end) = rest.find("?>") else {
                        return false;
                    };
                    text = rest[end + 2..].trim_start();
                } else {
                    break;
                }
            }
            text.strip_prefix("<svg").is_some_and(|rest| {
                rest.starts_with(|c: char| c.is_whitespace() || matches!(c, '>' | '/'))
            })
        })
}

// One atomic file: content type, newline, original bytes. No tokens, signed
// redirect URLs, or response cookies are persisted. Tempfiles are mode 0600.
fn write_cached(path: &Path, response: &BufferedResponse) -> anyhow::Result<()> {
    let Some(content_type) = image_content_type(response) else {
        return Ok(());
    };
    let dir = path.parent().unwrap();
    std::fs::create_dir_all(dir)?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    writeln!(file, "{content_type}")?;
    file.write_all(&response.bytes)?;
    file.persist(path)?;
    Ok(())
}

fn read_cached(path: &Path) -> Option<http::Response<AsyncBody>> {
    if std::fs::metadata(path).ok()?.len() > MAX_IMAGE_BYTES + 1024 {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES + 1024 {
        return None;
    }
    let end = bytes.iter().position(|b| *b == b'\n')?;
    let content_type = std::str::from_utf8(&bytes[..end]).ok()?;
    if !content_type.starts_with("image/") || bytes.len() <= end + 1 {
        return None;
    }
    http::Response::builder()
        .status(200)
        .header(http::header::CONTENT_TYPE, content_type)
        .body(AsyncBody::from(bytes[end + 1..].to_vec()))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use std::net::TcpListener;

    /// Serve one canned HTTP/1.1 response on an ephemeral port, once, and
    /// return the URL to hit it. Real wire format on a real socket — this
    /// is the seam that silently broke when gpui's client was Null, so it
    /// gets exercised end to end, not just in pieces.
    fn one_shot_server(status: &'static str, headers: &'static str, body: &'static str) -> String {
        server(status, headers, body.as_bytes()).0
    }

    fn server(status: &str, headers: &str, body: &[u8]) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (status, headers, body) = (status.to_owned(), headers.to_owned(), body.to_vec());
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let len = stream.read(&mut buf).unwrap();
            let response = format!(
                "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
            String::from_utf8_lossy(&buf[..len]).into_owned()
        });
        (url, worker)
    }

    fn test_client(dir: &std::path::Path) -> UreqHttpClient {
        let mut client = UreqHttpClient::new();
        client.cache_dir = Some(dir.to_owned());
        client.token = Arc::new(|| panic!("public/local images must not request a token"));
        client
    }

    fn response_bytes(response: http::Response<AsyncBody>) -> Vec<u8> {
        block_on(async {
            let mut bytes = Vec::new();
            response.into_body().read_to_end(&mut bytes).await.unwrap();
            bytes
        })
    }

    #[test]
    fn only_trusted_https_github_endpoints_get_a_token() {
        for url in [
            "https://github.com/user-attachments/assets/abc",
            "https://github.com/user-attachments/files/123/screenshot.svg",
            "https://github.com/acme/private/assets/123/abc",
            "https://github.com/acme/private/raw/main/image.png",
            "https://raw.githubusercontent.com/acme/private/main/image.svg",
        ] {
            assert!(needs_github_token(url), "{url}");
        }
        for url in [
            "http://github.com/user-attachments/assets/abc",
            "https://github.com:8443/user-attachments/assets/abc",
            "https://github.com.evil.test/user-attachments/assets/abc",
            "https://github.com@evil.test/user-attachments/assets/abc",
            "https://evil.test/github.com/user-attachments/assets/abc",
            "https://user:pass@github.com/user-attachments/assets/abc",
            "https://user-images.githubusercontent.com/123/image.png",
            "https://private-user-images.githubusercontent.com/123/image.png?jwt=signed",
            "https://avatars.githubusercontent.com/u/123",
            "https://github.com/login",
        ] {
            assert!(!needs_github_token(url), "{url}");
        }
    }

    #[test]
    fn private_attachment_authentication_reaches_the_wire() {
        let (url, worker) = server("200 OK", "Content-Type: image/png\r\n", b"PNG");
        let token: TokenProvider = Arc::new(|| Some("test-secret".into()));
        let request = authenticate(
            ureq::get(&url),
            "https://github.com/user-attachments/assets/private",
            &http::HeaderMap::new(),
            &token,
        );
        request.call().unwrap();
        assert!(worker
            .join()
            .unwrap()
            .to_lowercase()
            .contains("authorization: token test-secret"));

        let unused_token: TokenProvider = Arc::new(|| panic!("must not request a token"));
        let public = authenticate(
            ureq::get("https://example.com/image.png"),
            "https://example.com/image.png",
            &http::HeaderMap::new(),
            &unused_token,
        );
        assert!(public.header("Authorization").is_none());
        let headers = http::HeaderMap::from_iter([(
            http::header::AUTHORIZATION,
            "existing".parse().unwrap(),
        )]);
        let request = authenticate(
            ureq::get("https://github.com/user-attachments/assets/a")
                .set("Authorization", "existing"),
            "https://github.com/user-attachments/assets/a",
            &headers,
            &unused_token,
        );
        assert_eq!(request.header("Authorization"), Some("existing"));
    }

    #[test]
    fn redirects_never_forward_credentials_and_cache_under_the_stable_url() {
        let dir = tempfile::tempdir().unwrap();
        let client = test_client(dir.path());
        let (cdn, cdn_worker) = server("200 OK", "Content-Type: image/svg+xml\r\n", b"<svg/>");
        let (stable, origin_worker) = server("302 Found", &format!("Location: {cdn}\r\n"), b"");
        let request = http::Request::builder()
            .uri(&stable)
            .header("Authorization", "token test-secret")
            .extension(RedirectPolicy::FollowAll)
            .body(AsyncBody::empty())
            .unwrap();
        assert_eq!(
            response_bytes(block_on(client.send(request)).unwrap()),
            b"<svg/>"
        );
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
        let fresh_client = test_client(dir.path());
        assert_eq!(
            response_bytes(block_on(fresh_client.get(&stable, AsyncBody::empty(), true)).unwrap()),
            b"<svg/>"
        );
        let path = std::fs::read_dir(dir.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let contents = std::fs::read(&path).unwrap();
        assert!(!String::from_utf8_lossy(&contents).contains("test-secret"));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn concurrent_loads_and_a_fresh_client_reuse_disk_images_without_a_server() {
        let dir = tempfile::tempdir().unwrap();
        let client = test_client(dir.path());
        let (url, worker) = server("200 OK", "Content-Type: image/svg+xml\r\n", b"<svg/>");
        let (a, b) = block_on(async {
            futures::join!(
                client.get(&url, AsyncBody::empty(), true),
                client.get(&url, AsyncBody::empty(), true)
            )
        });
        assert_eq!(response_bytes(a.unwrap()), b"<svg/>");
        assert_eq!(response_bytes(b.unwrap()), b"<svg/>");
        worker.join().unwrap(); // server is now gone
        let fresh_client = test_client(dir.path());
        let cached = block_on(fresh_client.get(&url, AsyncBody::empty(), true)).unwrap();
        assert_eq!(cached.headers()["content-type"], "image/svg+xml");
        assert_eq!(response_bytes(cached), b"<svg/>");
    }

    #[test]
    fn cache_misses_errors_and_unwritable_caches_do_not_poison_images() {
        let dir = tempfile::tempdir().unwrap();
        let client = test_client(dir.path());
        for (status, content_type, body) in [
            ("404 Not Found", "image/png", "missing"),
            ("200 OK", "text/html", "<html>login</html>"),
            ("200 OK", "image/png", ""),
        ] {
            let (url, worker) = server(
                status,
                &format!("Content-Type: {content_type}\r\n"),
                body.as_bytes(),
            );
            response_bytes(block_on(client.get(&url, AsyncBody::empty(), true)).unwrap());
            worker.join().unwrap();
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        }
        let not_a_dir = dir.path().join("file");
        std::fs::write(&not_a_dir, "not a directory").unwrap();
        let client = test_client(&not_a_dir);
        let (url, worker) = server("200 OK", "Content-Type: image/svg+xml\r\n", b"<svg/>");
        assert_eq!(
            response_bytes(block_on(client.get(&url, AsyncBody::empty(), true)).unwrap()),
            b"<svg/>"
        );
        worker.join().unwrap();
    }

    #[test]
    fn corrupt_cache_is_refetched_and_mime_less_images_are_cached() {
        let dir = tempfile::tempdir().unwrap();
        let client = test_client(dir.path());
        let (url, worker) = server(
            "200 OK",
            "Content-Type: application/octet-stream\r\n",
            b"<!-- diagram --><svg/>",
        );
        let uri = url.parse::<http::Uri>().unwrap().to_string();
        let path = dir
            .path()
            .join(format!("{:x}", Sha256::digest(uri.as_bytes())));
        std::fs::write(&path, b"corrupt").unwrap();
        response_bytes(block_on(client.get(&url, AsyncBody::empty(), true)).unwrap());
        worker.join().unwrap();
        assert_eq!(
            response_bytes(
                block_on(test_client(dir.path()).get(&url, AsyncBody::empty(), true)).unwrap()
            ),
            b"<!-- diagram --><svg/>"
        );
        assert!(read_cached(&path).is_some());
    }

    /// Through the same `get(…, follow_redirects: true)` path gpui's image
    /// loader uses: bytes in, status and headers out.
    #[test]
    fn round_trips_success_responses() {
        let url = one_shot_server("200 OK", "Content-Type: text/plain\r\n", "hello");
        let response = block_on(http_client().get(&url, AsyncBody::empty(), true)).unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["Content-Type"], "text/plain");
        let mut bytes = Vec::new();
        let mut body = response.into_body();
        block_on(async {
            body.read_to_end(&mut bytes).await.unwrap();
        });
        assert_eq!(bytes, b"hello");
    }

    /// Non-2xx statuses are responses, not errors — the image loader checks
    /// `status().is_success()` itself and logs the status when it isn't.
    #[test]
    fn passes_non_2xx_statuses_through_as_responses() {
        let url = one_shot_server("404 Not Found", "", "missing");
        let response = block_on(http_client().get(&url, AsyncBody::empty(), true)).unwrap();
        assert_eq!(response.status(), 404);
    }

    #[test]
    fn no_follow_keeps_redirects_as_responses() {
        let url = one_shot_server(
            "302 Found",
            "Location: http://127.0.0.1:0/image.png\r\n",
            "",
        );
        let response = block_on(http_client().get(&url, AsyncBody::empty(), false)).unwrap();
        assert_eq!(response.status(), 302);
    }

    /// Connection-refused surfaces as an Err, not a hang or a panic.
    #[test]
    fn reports_unreachable_servers() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener); // nothing is listening there
        let result = block_on(http_client().get(&url, AsyncBody::empty(), true));
        assert!(result.is_err());
    }
}
