//! The app's HTTP client, registered with gpui at startup via
//! `with_http_client`. gpui itself ships a `NullHttpClient` — without this
//! module, every remote `img()` (the account avatar today, PR media later)
//! silently fails to load and renders as the Avatar's empty circle.
//!
//! Implementation: ureq — blocking, rustls, a tiny dependency tree (no
//! tokio/hyper) — run on a short-lived thread per request and bridged into
//! gpui's async world through a oneshot channel. v0.1's traffic is small
//! image fetches, where a thread per request beats a resident runtime.

use futures::channel::oneshot;
use futures::future::BoxFuture;
use futures::{AsyncReadExt as _, FutureExt as _};
use http_client::{http, AsyncBody, HttpClient, RedirectPolicy, Url};
use std::io::Read as _;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Time budget for one request, connect and response combined. Image
/// fetches are small; anything longer is a stuck connection not worth
/// holding a thread for.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

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
                .build()
        }
        Self {
            follow: agent(10),
            no_follow: agent(0),
        }
    }
}

impl HttpClient for UreqHttpClient {
    fn type_name(&self) -> &'static str {
        "ureq"
    }

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
        async move {
            // The body must be read off the request before it can cross to
            // a blocking thread. GETs carry `AsyncBody::empty`, which reads
            // zero bytes immediately.
            let (parts, mut body) = req.into_parts();
            let mut req_body = Vec::new();
            body.read_to_end(&mut req_body).await?;

            let (tx, rx) = oneshot::channel();
            thread::spawn(move || {
                let _ = tx.send(run(agent, parts, req_body));
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
) -> anyhow::Result<http::Response<AsyncBody>> {
    let url = parts.uri.to_string();
    let mut request = agent.request(parts.method.as_str(), &url);
    for (name, value) in parts.headers.iter() {
        // ureq speaks &str headers; a non-UTF-8 value has no way across,
        // so it's skipped rather than failing the whole request.
        if let Ok(value) = value.to_str() {
            request = request.set(name.as_str(), value);
        }
    }
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

    let mut builder = http::Response::builder().status(response.status());
    for name in response.headers_names() {
        if let Some(value) = response.header(&name) {
            builder = builder.header(&name, value);
        }
    }
    // Everything this app fetches is small (avatars, later media blobs):
    // buffer the whole response rather than streaming it.
    let mut bytes = Vec::new();
    response.into_reader().read_to_end(&mut bytes)?;
    Ok(builder.body(AsyncBody::from(bytes))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use std::io::Write as _;
    use std::net::TcpListener;

    /// Serve one canned HTTP/1.1 response on an ephemeral port, once, and
    /// return the URL to hit it. Real wire format on a real socket — this
    /// is the seam that silently broke when gpui's client was Null, so it
    /// gets exercised end to end, not just in pieces.
    fn one_shot_server(status: &'static str, headers: &'static str, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf); // consume the request line
            let response = format!(
                "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        url
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
