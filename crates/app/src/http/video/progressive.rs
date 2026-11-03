//! A private loopback range server over a bounded sparse file. AVFoundation sees
//! neither GitHub credentials nor signed CDN URLs. Previews fetch bounded ranges;
//! Play starts one eager full download, independently of decoder buffering.
use super::*;
use anyhow::Context as _;
use std::{
    collections::HashSet,
    fs::File,
    io::{Seek, SeekFrom},
    net::{TcpListener, TcpStream},
    sync::Weak,
};

const CHUNK: u64 = 1024 * 1024;
const PRELOAD_CHUNKS: usize = 8;

pub(super) struct Cache {
    origin: String,
    token: TokenProvider,
    file: Mutex<File>,
    chunks: Mutex<HashSet<u64>>,
    fetch: Mutex<()>,
    total: u64,
    mime: String,
    ranges: bool,
    transfer: Arc<Transfer>,
    endpoint: String,
    address: std::net::SocketAddr,
    path: String,
    _directory: tempfile::TempDir,
}

pub(super) fn prepare(url: String, transfer: Arc<Transfer>) -> anyhow::Result<VideoFile> {
    transfer.check()?;
    ensure!(safe_url(&url), "Only HTTP(S) video sources are supported");
    let token = UreqHttpClient::new().token;
    let response = request(&url, &token, Some((0, CHUNK - 1)))?;
    let mime = response
        .header("Content-Type")
        .unwrap_or_default()
        .to_owned();
    ensure!(
        is_video_type(&mime) || (direct_video(&url) && mime == "application/octet-stream"),
        "Unsupported video response (the server may require sign-in)"
    );
    let ranges = response.status() == 206;
    let total = if ranges {
        let (start, end, total) = content_range(&response)?;
        ensure!(
            start == 0 && end == (CHUNK - 1).min(total - 1),
            "Invalid video range response"
        );
        total
    } else {
        response
            .header("Content-Length")
            .and_then(|s| s.parse().ok())
            .context("Video server did not provide a file size")?
    };
    ensure!(
        total > 0 && total <= MAX_VIDEO_BYTES,
        "Video exceeds 128 MiB download limit or is empty"
    );
    transfer.total.store(total, Ordering::Relaxed);
    // Do not allocate a temporary file while waiting for initial network data.
    let mut first = vec![0; total.min(CHUNK) as usize];
    response.into_reader().read_exact(&mut first)?;
    transfer.check()?;
    let directory = tempfile::Builder::new().prefix("gander-video-").tempdir()?;
    let path = directory
        .path()
        .join(format!("video.{}", cache_extension(&mime, &url)));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    file.set_len(total)?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let mut random = [0; 32];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    let secret: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let route = format!("/{secret}/video.{}", cache_extension(&mime, &url));
    let cache = Arc::new(Cache {
        origin: url,
        token,
        file: Mutex::new(file),
        chunks: Mutex::default(),
        fetch: Mutex::new(()),
        total,
        mime,
        ranges,
        transfer,
        endpoint: format!("http://{}{route}", listener.local_addr()?),
        address: listener.local_addr()?,
        path: route,
        _directory: directory,
    });
    cache.store(0, &first)?;
    let weak = Arc::downgrade(&cache);
    std::thread::spawn(move || {
        let clients = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        loop {
            if weak
                .upgrade()
                .is_none_or(|c| c.transfer.canceled.load(Ordering::Relaxed))
            {
                break;
            }
            match listener.accept() {
                Ok((socket, _)) => {
                    if clients.load(Ordering::Relaxed) >= 4 {
                        drop(socket);
                        continue;
                    }
                    clients.fetch_add(1, Ordering::Relaxed);
                    let clients = clients.clone();
                    let weak = weak.clone();
                    std::thread::spawn(move || {
                        serve(socket, weak);
                        clients.fetch_sub(1, Ordering::Relaxed);
                    });
                }
                Err(_) => break,
            }
        }
    });
    Ok(VideoFile {
        path,
        progressive: cache,
    })
}
use std::os::unix::fs::OpenOptionsExt;

fn request(
    url: &str,
    token: &TokenProvider,
    range: Option<(u64, u64)>,
) -> anyhow::Result<ureq::Response> {
    let mut req = agent().get(url).set("Accept-Encoding", "identity");
    if let Some((start, end)) = range {
        req = req.set("Range", &format!("bytes={start}-{end}"));
    }
    authenticate(req, url, &http::HeaderMap::new(), token)
        .call()
        .map_err(|_| anyhow::anyhow!("Could not load video"))
}
fn content_range(response: &ureq::Response) -> anyhow::Result<(u64, u64, u64)> {
    let value = response
        .header("Content-Range")
        .and_then(|s| s.strip_prefix("bytes "))
        .context("Missing video range metadata")?;
    let (range, total) = value
        .split_once('/')
        .context("Invalid video range metadata")?;
    let (start, end) = range
        .split_once('-')
        .context("Invalid video range metadata")?;
    let (start, end, total) = (
        start.parse::<u64>()?,
        end.parse::<u64>()?,
        total.parse::<u64>()?,
    );
    ensure!(
        start <= end && end < total && total <= MAX_VIDEO_BYTES,
        "Invalid video range metadata"
    );
    Ok((start, end, total))
}
impl Cache {
    pub(super) fn endpoint(&self) -> &str {
        &self.endpoint
    }
    pub(super) fn cancel(&self) {
        self.transfer.cancel();
        // Wake blocking accept without polling while a preview sits paused.
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_millis(10));
        // POSIX unlink is safe with open readers/writers. Remove names now,
        // not after a blocked network worker exits (especially at app quit).
        let _ = std::fs::remove_dir_all(self._directory.path());
    }
    fn store(&self, chunk: u64, bytes: &[u8]) -> anyhow::Result<()> {
        self.transfer.check()?;
        let mut file = self.file.lock().unwrap();
        file.seek(SeekFrom::Start(chunk * CHUNK))?;
        file.write_all(bytes)?;
        if self.chunks.lock().unwrap().insert(chunk) {
            self.transfer
                .bytes
                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
        if self.transfer.bytes.load(Ordering::Relaxed) == self.total {
            self.transfer.complete.store(true, Ordering::Release);
        }
        self.transfer.wake();
        Ok(())
    }
    fn has(&self, chunk: u64) -> bool {
        self.chunks.lock().unwrap().contains(&chunk)
    }
    fn may_fetch(&self, chunk: u64) -> bool {
        let chunks = self.chunks.lock().unwrap();
        if !self.transfer.started.load(Ordering::Acquire) {
            return chunks.len() < PRELOAD_CHUNKS;
        }
        // The eager connection is already delivering near-future playback data.
        // Don't stall it (or the decoder) behind a duplicate range round-trip.
        // Far-away seeks still get a priority range instead of waiting for the
        // sequential download to reach them.
        let next = (0..self.total.div_ceil(CHUNK))
            .find(|i| !chunks.contains(i))
            .unwrap_or(self.total.div_ceil(CHUNK));
        chunk > next + 2
    }
    fn ensure(&self, chunk: u64) -> anyhow::Result<()> {
        loop {
            self.transfer.check()?;
            ensure!(
                self.transfer.error.lock().unwrap().is_none(),
                "Video download failed"
            );
            if self.has(chunk) {
                return Ok(());
            }
            let started = self.transfer.started.load(Ordering::Acquire);
            if self.may_fetch(chunk) && self.ranges {
                let _guard = self.fetch.lock().unwrap();
                if self.has(chunk) {
                    return Ok(());
                }
                if !self.may_fetch(chunk) {
                    continue;
                }
                let start = chunk * CHUNK;
                let end = (start + CHUNK).min(self.total) - 1;
                let response = request(&self.origin, &self.token, Some((start, end)))?;
                ensure!(
                    response.status() == 206
                        && content_range(&response)? == (start, end, self.total),
                    "Video range changed while loading"
                );
                let mut bytes = vec![0; (end - start + 1) as usize];
                response.into_reader().read_exact(&mut bytes)?;
                return self.store(chunk, &bytes);
            }
            let event = self.transfer.event.lock().unwrap();
            if self.has(chunk)
                || self.transfer.canceled.load(Ordering::Relaxed)
                || self.transfer.started.load(Ordering::Acquire) != started
                || self.transfer.error.lock().unwrap().is_some()
            {
                continue;
            }
            let _wait = self
                .transfer
                .changed
                .wait_timeout(event, Duration::from_secs(1))
                .unwrap();
        }
    }
    pub(super) fn start(self: &Arc<Self>) {
        if self.transfer.started.swap(true, Ordering::AcqRel)
            || self.transfer.complete.load(Ordering::Acquire)
        {
            return;
        }
        self.transfer.wake();
        let cache = self.clone();
        std::thread::spawn(move || {
            let result = (|| {
                cache.transfer.check()?;
                let response = request(&cache.origin, &cache.token, None)?;
                ensure!(
                    response.status() == 200
                        && response
                            .header("Content-Type")
                            .is_some_and(|mime| mime.eq_ignore_ascii_case(&cache.mime))
                        && response
                            .header("Content-Length")
                            .and_then(|s| s.parse::<u64>().ok())
                            == Some(cache.total),
                    "Video size changed while downloading"
                );
                let mut reader = response.into_reader();
                for chunk in 0..cache.total.div_ceil(CHUNK) {
                    cache.transfer.check()?;
                    let mut bytes = vec![0; CHUNK.min(cache.total - chunk * CHUNK) as usize];
                    reader.read_exact(&mut bytes)?;
                    cache.store(chunk, &bytes)?;
                }
                Ok::<_, anyhow::Error>(())
            })();
            if result.is_err() && !cache.transfer.canceled.load(Ordering::Relaxed) {
                *cache.transfer.error.lock().unwrap() =
                    Some("Could not finish downloading video. Retry to reconnect.".into());
                cache.transfer.wake();
            }
        });
    }
}
fn serve(mut socket: TcpStream, weak: Weak<Cache>) {
    let _ = socket.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = socket.set_write_timeout(Some(Duration::from_secs(5)));
    let Some(cache) = weak.upgrade() else {
        return;
    };
    let result = (|| -> anyhow::Result<()> {
        let mut header = Vec::new();
        let mut b = [0; 1];
        while header.len() < 8192 && !header.ends_with(b"\r\n\r\n") {
            socket.read_exact(&mut b)?;
            header.push(b[0]);
        }
        let header = std::str::from_utf8(&header)?;
        let mut lines = header.lines();
        let mut first = lines.next().unwrap_or_default().split_whitespace();
        let method = first.next().unwrap_or_default();
        let route = first.next().unwrap_or_default();
        if route != cache.path || !matches!(method, "GET" | "HEAD") {
            socket.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )?;
            return Ok(());
        }
        let range = lines.find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("range").then(|| value.trim())
        });
        let bounds = range.map(|s| parse_range(s, cache.total)).transpose();
        let (start, end) = match bounds {
            Ok(Some(r)) => r,
            Ok(None) => (0, cache.total - 1),
            Err(_) => {
                write!(socket, "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", cache.total)?;
                return Ok(());
            }
        };
        write!(socket, "HTTP/1.1 {}\r\nContent-Type: {}\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n", if range.is_some() { "206 Partial Content" } else { "200 OK" }, cache.mime, end - start + 1)?;
        if range.is_some() {
            write!(
                socket,
                "Content-Range: bytes {start}-{end}/{}\r\n",
                cache.total
            )?;
        }
        socket.write_all(b"\r\n")?;
        if method == "HEAD" {
            return Ok(());
        }
        let mut offset = start;
        while offset <= end {
            let chunk = offset / CHUNK;
            cache.ensure(chunk)?;
            let n = (end - offset + 1).min(CHUNK - offset % CHUNK) as usize;
            let mut bytes = vec![0; n];
            {
                let mut file = cache.file.lock().unwrap();
                file.seek(SeekFrom::Start(offset))?;
                file.read_exact(&mut bytes)?;
            }
            socket.write_all(&bytes)?;
            offset += n as u64;
        }
        Ok(())
    })();
    // Disconnects (e.g. AVFoundation abandoning a speculative range) are normal.
    let _ = result;
}
#[cfg(test)]
#[path = "progressive_tests.rs"]
mod tests;

fn parse_range(value: &str, total: u64) -> anyhow::Result<(u64, u64)> {
    let (start, end) = value
        .strip_prefix("bytes=")
        .and_then(|s| s.split_once('-'))
        .context("Invalid range")?;
    let (start, end) = if start.is_empty() {
        let n = end.parse::<u64>()?;
        ensure!(n > 0, "Empty range");
        (total.saturating_sub(n), total - 1)
    } else {
        (
            start.parse::<u64>()?,
            if end.is_empty() {
                total - 1
            } else {
                end.parse::<u64>()?.min(total - 1)
            },
        )
    };
    ensure!(start <= end && start < total, "Unsatisfiable range");
    Ok((start, end))
}
