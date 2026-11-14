//! Bounded CLI reads shared by Git, GitHub and the app. The deadline includes
//! admission and pipe draining; truncated output is always an error.

use std::io;
use std::process::{Command, Output};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

pub const MAX_READ_PROCESSES: usize = 4;
pub const TEXT_BYTES: usize = 1024 * 1024;
pub const JSON_BYTES: usize = 8 * 1024 * 1024;
pub const PATCH_BYTES: usize = 32 * 1024 * 1024;
pub const BINARY_BYTES: usize = 100 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Read,
    /// A remote write must never be retried on a lost/oversized response.
    Mutation,
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub timeout: Duration,
    pub stdout: usize,
    pub stderr: usize,
    pub operation: Operation,
}

impl Limits {
    pub const fn read(stdout: usize) -> Self {
        Self {
            timeout: Duration::from_secs(30),
            stdout,
            stderr: 64 * 1024,
            operation: Operation::Read,
        }
    }
}

#[derive(Debug)]
struct Incomplete {
    reason: &'static str,
    uncertain: bool,
}

impl std::fmt::Display for Incomplete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.reason)?;
        if self.uncertain {
            write!(
                f,
                "; the write may have succeeded; refresh before trying again"
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for Incomplete {}

fn incomplete(limits: Limits, kind: io::ErrorKind, reason: &'static str) -> io::Error {
    io::Error::new(
        kind,
        Incomplete {
            reason,
            uncertain: limits.operation == Operation::Mutation,
        },
    )
}

struct ReadSlots {
    active: Mutex<usize>,
    changed: Condvar,
    limit: usize,
}

impl ReadSlots {
    const fn new(limit: usize) -> Self {
        Self {
            active: Mutex::new(0),
            changed: Condvar::new(),
            limit,
        }
    }

    fn acquire(&self, deadline: Instant) -> io::Result<Permit<'_>> {
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out waiting for a process slot",
                    )
                })?;
            if *active < self.limit {
                *active += 1;
                return Ok(Permit(self));
            }
            active = self
                .changed
                .wait_timeout(active, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

struct Permit<'a>(&'a ReadSlots);

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        *self.0.active.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
        self.0.changed.notify_one();
    }
}

static READ_SLOTS: ReadSlots = ReadSlots::new(MAX_READ_PROCESSES);

pub fn run(command: &mut Command, limits: Limits) -> io::Result<Output> {
    run_inner(command, limits, None, &READ_SLOTS)
}

/// Binary snapshots stream to their existing temporary file, with the same
/// deadline, concurrency and byte bounds as captured stdout. Discard on error.
pub fn run_to_file(
    command: &mut Command,
    limits: Limits,
    file: &mut std::fs::File,
) -> io::Result<Output> {
    run_inner(command, limits, Some(file), &READ_SLOTS)
}

#[cfg(not(unix))]
fn run_inner(
    _: &mut Command,
    _: Limits,
    _: Option<&mut std::fs::File>,
    _: &ReadSlots,
) -> io::Result<Output> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "bounded subprocesses require Unix",
    ))
}

#[cfg(unix)]
fn run_inner(
    command: &mut Command,
    limits: Limits,
    mut file: Option<&mut std::fs::File>,
    slots: &ReadSlots,
) -> io::Result<Output> {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Stdio};

    struct Job(Child);
    impl Drop for Job {
        fn drop(&mut self) {
            // Each invocation owns its process group, including credential
            // helpers and children that inherited a pipe after its parent quit.
            // SAFETY: process_group(0) made the child's PID its group ID.
            unsafe {
                libc::kill(-(self.0.id() as libc::pid_t), libc::SIGKILL);
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
        // SAFETY: the owned pipe FD is valid, and F_GETFL/F_SETFL take no pointers.
        let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
        if flags == -1
            || unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                == -1
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn drain(
        pipe: &mut impl Read,
        captured: &mut Vec<u8>,
        mut file: Option<&mut std::fs::File>,
        total: &mut usize,
        cap: usize,
    ) -> io::Result<(bool, bool)> {
        let mut buffer = [0; 8192];
        let mut progress = false;
        // Alternate streams and check the deadline even for an endless writer.
        for _ in 0..8 {
            match pipe.read(&mut buffer) {
                Ok(0) => return Ok((true, progress)),
                Ok(n) => {
                    if n > cap.saturating_sub(*total) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "output limit exceeded",
                        ));
                    }
                    *total += n;
                    if let Some(file) = file.as_mut() {
                        file.write_all(&buffer[..n])?;
                    } else {
                        captured.extend_from_slice(&buffer[..n]);
                    }
                    progress = true;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok((false, progress))
    }

    let deadline = Instant::now() + limits.timeout;
    let _permit = if limits.operation == Operation::Read {
        Some(slots.acquire(deadline)?)
    } else {
        None
    };
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "process deadline expired before starting",
        ));
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = Job(command.spawn()?);
    let mut stdout = child.0.stdout.take().unwrap();
    let mut stderr = child.0.stderr.take().unwrap();
    nonblocking(&stdout)?;
    nonblocking(&stderr)?;
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let (mut out_bytes, mut err_bytes) = (0, 0);
    let (mut out_eof, mut err_eof) = (false, false);
    loop {
        if Instant::now() >= deadline {
            return Err(incomplete(
                limits,
                io::ErrorKind::TimedOut,
                "process timed out",
            ));
        }
        let mut progress = false;
        if !out_eof {
            let (eof, read) = drain(
                &mut stdout,
                &mut out,
                file.as_deref_mut(),
                &mut out_bytes,
                limits.stdout,
            )
            .map_err(|_| {
                incomplete(
                    limits,
                    io::ErrorKind::InvalidData,
                    "stdout exceeded its limit or could not be read",
                )
            })?;
            out_eof = eof;
            progress |= read;
        }
        if !err_eof {
            let (eof, read) = drain(&mut stderr, &mut err, None, &mut err_bytes, limits.stderr)
                .map_err(|_| {
                    incomplete(
                        limits,
                        io::ErrorKind::InvalidData,
                        "stderr exceeded its limit or could not be read",
                    )
                })?;
            err_eof = eof;
            progress |= read;
        }
        // Keep an exited parent unreaped while a descendant holds its pipes.
        // That reserves the PID/group ID until cleanup, rather than leaving a
        // potentially reused ID around for the rest of the deadline.
        if out_eof && err_eof {
            if let Some(status) = child.0.try_wait()? {
                return Ok(Output {
                    status,
                    stdout: out,
                    stderr: err,
                });
            }
        }
        if !progress {
            let mut pipes = [
                libc::pollfd {
                    fd: if out_eof { -1 } else { stdout.as_raw_fd() },
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if err_eof { -1 } else { stderr.as_raw_fd() },
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let wait_ms = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(5) as i32;
            // SAFETY: the array contains exactly two valid pollfd entries; all
            // live FDs remain owned here, and poll never writes outside it.
            if unsafe { libc::poll(pipes.as_mut_ptr(), pipes.len() as libc::nfds_t, wait_ms) } == -1
            {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    fn shell(script: &str) -> Command {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", script]);
        cmd
    }

    #[test]
    fn drains_both_pipes_without_deadlock_and_rejects_either_overflow() {
        let script = "head -c 131072 /dev/zero & head -c 131072 /dev/zero >&2; wait";
        let mut limits = Limits::read(131072);
        limits.stderr = 131072;
        let out = run(&mut shell(script), limits).unwrap();
        assert_eq!(out.stdout.len(), 131072);
        assert_eq!(out.stderr.len(), 131072);
        for (stdout, stderr) in [(131071, 131072), (131072, 131071), (8192, 8192)] {
            limits.stdout = stdout;
            limits.stderr = stderr;
            assert_eq!(
                run(&mut shell(script), limits).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn deadline_covers_parent_exit_and_descendant_pipe_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("escaped");
        let mut cmd = shell("(sleep 1; touch \"$1\") & exit 0");
        cmd.arg("fixture").arg(&marker);
        let mut limits = Limits::read(1024);
        limits.timeout = Duration::from_millis(100);
        let start = Instant::now();
        assert_eq!(
            run(&mut cmd, limits).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        std::thread::sleep(Duration::from_millis(1100));
        assert!(
            !marker.exists(),
            "descendant survived process-group cleanup"
        );
    }

    #[test]
    fn timeout_kills_and_reaps_the_direct_child() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let mut cmd = shell("echo $$ > \"$1\"; exec sleep 30");
        cmd.arg("fixture").arg(&pidfile);
        let mut limits = Limits::read(1024);
        limits.timeout = Duration::from_millis(100);
        assert_eq!(
            run(&mut cmd, limits).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        let pid: i32 = std::fs::read_to_string(pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: signal zero only probes existence; it cannot signal another job.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }

    #[test]
    fn admission_is_bounded_and_queued_deadlines_expire_without_spawning() {
        let slots = Arc::new(ReadSlots::new(2));
        let barrier = Arc::new(Barrier::new(3));
        std::thread::scope(|scope| {
            for _ in 0..2 {
                let slots = slots.clone();
                let barrier = barrier.clone();
                scope.spawn(move || {
                    let _permit = slots
                        .acquire(Instant::now() + Duration::from_secs(2))
                        .unwrap();
                    barrier.wait();
                    std::thread::sleep(Duration::from_millis(150));
                });
            }
            barrier.wait();
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join("spawned");
            let mut cmd = shell("touch \"$1\"");
            cmd.arg("fixture").arg(&marker);
            let mut limits = Limits::read(1024);
            limits.timeout = Duration::from_millis(50);
            assert_eq!(
                run_inner(&mut cmd, limits, None, &slots)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::TimedOut
            );
            assert!(!marker.exists());
            assert_eq!(*slots.active.lock().unwrap(), 2);
        });
        assert_eq!(*slots.active.lock().unwrap(), 0);
        assert!(run_inner(
            &mut shell("printf admitted"),
            Limits::read(1024),
            None,
            &slots
        )
        .unwrap()
        .status
        .success());
    }

    #[test]
    fn mutation_timeout_reports_an_uncertain_write_without_retrying() {
        let mut limits = Limits::read(1024);
        limits.operation = Operation::Mutation;
        limits.timeout = Duration::from_millis(50);
        let err = run(&mut shell("exec sleep 30"), limits).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(err.to_string().contains("write may have succeeded"));
    }

    #[test]
    fn concurrent_fixture_processes_share_the_same_read_bound() {
        let slots = ReadSlots::new(2);
        let dir = tempfile::tempdir().unwrap();
        let events = dir.path().join("events");
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let events = &events;
                let slots = &slots;
                scope.spawn(move || {
                    let mut cmd =
                        shell("printf 'start\\n' >> \"$1\"; sleep 0.03; printf 'end\\n' >> \"$1\"");
                    cmd.arg("fixture").arg(events);
                    assert!(run_inner(&mut cmd, Limits::read(1024), None, slots)
                        .unwrap()
                        .status
                        .success());
                });
            }
        });
        let (mut active, mut peak, mut starts) = (0, 0, 0);
        for event in std::fs::read_to_string(events).unwrap().lines() {
            match event {
                "start" => {
                    active += 1;
                    starts += 1;
                    peak = peak.max(active);
                }
                "end" => active -= 1,
                _ => panic!("unexpected fixture event"),
            }
            assert!((0..=2).contains(&active));
        }
        assert_eq!((active, peak, starts), (0, 2, 8));
    }

    #[test]
    fn streamed_binary_snapshots_are_bounded_too() {
        let mut file = tempfile::tempfile().unwrap();
        let mut limits = Limits::read(131072);
        let out = run_to_file(&mut shell("head -c 131072 /dev/zero"), limits, &mut file).unwrap();
        assert!(out.stdout.is_empty());
        assert_eq!(file.metadata().unwrap().len(), 131072);
        limits.stdout -= 1;
        assert_eq!(
            run_to_file(&mut shell("head -c 131072 /dev/zero"), limits, &mut file)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
}
