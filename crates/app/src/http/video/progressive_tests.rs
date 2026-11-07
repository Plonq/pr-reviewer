use super::*;
use std::sync::atomic::AtomicUsize;

struct Origin {
    url: String,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<Option<String>>>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Origin {
    fn new(total: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/clip.mp4", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let worker = std::thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        let seen = seen.clone();
                        std::thread::spawn(move || {
                            // macOS inherits the listener's nonblocking mode. Wait for
                            // request bytes instead of closing on a transient WouldBlock.
                            socket.set_nonblocking(false).unwrap();
                            socket
                                .set_read_timeout(Some(Duration::from_secs(3)))
                                .unwrap();
                            socket
                                .set_write_timeout(Some(Duration::from_secs(3)))
                                .unwrap();
                            let mut bytes = Vec::new();
                            let mut b = [0; 1];
                            while !bytes.ends_with(b"\r\n\r\n") {
                                if socket.read_exact(&mut b).is_err() {
                                    return;
                                }
                                bytes.push(b[0]);
                            }
                            let request = String::from_utf8(bytes).unwrap();
                            assert!(request.starts_with("GET "));
                            let range = request.lines().find_map(|line| {
                                let (key, value) = line.split_once(':')?;
                                key.eq_ignore_ascii_case("range")
                                    .then(|| value.trim().to_owned())
                            });
                            seen.lock().unwrap().push(range.clone());
                            let (start, end) = range
                                .as_deref()
                                .map(|r| parse_range(r, total as u64).unwrap())
                                .unwrap_or((0, total as u64 - 1));
                            let status = if range.is_some() {
                                "206 Partial Content"
                            } else {
                                "200 OK"
                            };
                            let _ = write!(socket, "HTTP/1.1 {status}\r\nContent-Type: video/mp4\r\nContent-Length: {}\r\nConnection: close\r\n", end - start + 1);
                            if range.is_some() {
                                let _ = write!(
                                    socket,
                                    "Content-Range: bytes {start}-{end}/{total}\r\n"
                                );
                            }
                            let _ = socket.write_all(b"\r\n");
                            let mut at = start;
                            while at <= end {
                                let n = (end - at + 1).min(64 * 1024) as usize;
                                let bytes: Vec<_> =
                                    (at..at + n as u64).map(|i| (i % 251) as u8).collect();
                                if socket.write_all(&bytes).is_err() {
                                    break;
                                }
                                at += n as u64;
                            }
                        });
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(2)),
                }
            }
        });
        Self {
            url,
            stop,
            requests,
            worker: Some(worker),
        }
    }
}
impl Drop for Origin {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}
fn until(mut ready: impl FnMut() -> bool) {
    let start = std::time::Instant::now();
    while !ready() {
        assert!(start.elapsed() < Duration::from_secs(8), "timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}
#[test]
fn preview_ranges_are_cached_and_play_eagerly_finishes_even_without_a_reader() {
    let origin = Origin::new(12 * CHUNK as usize + 19);
    let transfer = Arc::<Transfer>::default();
    let file = prepare(origin.url.clone(), transfer.clone()).unwrap();
    let endpoint = file.source().to_owned();
    assert_eq!(transfer.bytes.load(Ordering::Relaxed), CHUNK);
    assert!(!transfer.started.load(Ordering::Acquire));
    let start = transfer.total.load(Ordering::Relaxed) - 50;
    let response = agent()
        .get(&endpoint)
        .set("Range", &format!("bytes={start}-"))
        .call()
        .unwrap();
    assert_eq!(response.status(), 206);
    let mut tail = Vec::new();
    response.into_reader().read_to_end(&mut tail).unwrap();
    assert_eq!(
        tail,
        (start..start + 50)
            .map(|i| (i % 251) as u8)
            .collect::<Vec<_>>()
    );
    assert!(transfer.bytes.load(Ordering::Relaxed) < transfer.total.load(Ordering::Relaxed));
    let requests = origin.requests.lock().unwrap().len();
    let mut again = Vec::new();
    agent()
        .get(&endpoint)
        .set("Range", &format!("bytes={start}-"))
        .call()
        .unwrap()
        .into_reader()
        .read_to_end(&mut again)
        .unwrap();
    assert_eq!(origin.requests.lock().unwrap().len(), requests);
    assert_eq!(tail, again);
    file.start(); // No decoder reader remains: eager completion is independent.
    file.progressive.ensure(1).unwrap(); // Near-future data comes from the eager stream, not a duplicate range GET.
    until(|| transfer.complete.load(Ordering::Acquire));
    assert_eq!(
        origin
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.is_some())
            .count(),
        requests
    );
    assert!(origin.requests.lock().unwrap().iter().any(|r| r.is_none()));
    let bytes = std::fs::read(&file.path).unwrap();
    assert_eq!(bytes.len() as u64, transfer.total.load(Ordering::Relaxed));
    assert!(bytes.iter().enumerate().all(|(i, b)| *b == (i % 251) as u8));
    let path = file.path.clone();
    drop(file);
    until(|| !path.exists());
}
#[test]
fn preload_budget_blocks_more_ranges_until_play_or_cancellation() {
    let origin = Origin::new(12 * CHUNK as usize);
    let transfer = Arc::<Transfer>::default();
    let file = prepare(origin.url.clone(), transfer.clone()).unwrap();
    let cache = file.progressive.clone();
    for chunk in 1..PRELOAD_CHUNKS as u64 {
        cache.ensure(chunk).unwrap();
    }
    let finished = Arc::new(AtomicUsize::new(0));
    let counted = finished.clone();
    let pending = cache.clone();
    let worker = std::thread::spawn(move || {
        assert!(pending.ensure(PRELOAD_CHUNKS as u64).is_err());
        counted.fetch_add(1, Ordering::Relaxed);
    });
    std::thread::sleep(Duration::from_millis(80));
    assert_eq!(
        transfer.bytes.load(Ordering::Relaxed),
        PRELOAD_CHUNKS as u64 * CHUNK
    );
    assert_eq!(origin.requests.lock().unwrap().len(), PRELOAD_CHUNKS);
    assert_eq!(finished.load(Ordering::Relaxed), 0);
    transfer.cancel();
    worker.join().unwrap();
    assert_eq!(finished.load(Ordering::Relaxed), 1);
    drop(cache);
    let path = file.path.clone();
    drop(file);
    until(|| !path.exists());
}
#[test]
fn failed_eager_download_is_reported_and_never_marked_complete() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/clip.mp4", listener.local_addr().unwrap());
    let worker = std::thread::spawn(move || {
        for first in [true, false] {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).unwrap();
            if first {
                write!(socket, "HTTP/1.1 206 Partial Content\r\nContent-Type: video/mp4\r\nContent-Range: bytes 0-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", CHUNK - 1, CHUNK * 2, CHUNK).unwrap();
                socket.write_all(&vec![7; CHUNK as usize]).unwrap();
            } else {
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Length: {}\r\nConnection: close\r\n\r\nshort", CHUNK * 2).unwrap();
            }
        }
    });
    let transfer = Arc::<Transfer>::default();
    let file = prepare(url, transfer.clone()).unwrap();
    file.start();
    until(|| transfer.error.lock().unwrap().is_some());
    assert!(!transfer.complete.load(Ordering::Acquire));
    assert_eq!(transfer.bytes.load(Ordering::Relaxed), CHUNK);
    drop(file);
    worker.join().unwrap();
}

#[test]
fn local_endpoint_rejects_unknown_paths_and_invalid_ranges() {
    let origin = Origin::new(2 * CHUNK as usize);
    let file = prepare(origin.url.clone(), Arc::default()).unwrap();
    let source = file.source();
    assert!(matches!(
        agent().get(&format!("{source}/wrong")).call(),
        Err(ureq::Error::Status(404, _))
    ));
    for range in [
        "bytes=99999999-",
        "bytes=10-2",
        "bytes=0-1,4-5",
        "items=0-1",
    ] {
        assert!(matches!(
            agent().get(source).set("Range", range).call(),
            Err(ureq::Error::Status(416, _))
        ));
    }
    assert_eq!(parse_range("bytes=-10", 100).unwrap(), (90, 99));
    assert_eq!(parse_range("bytes=20-200", 100).unwrap(), (20, 99));
}
