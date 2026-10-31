//! Finder/Dock launches don't inherit a terminal's shell setup. Recover PATH
//! once, before threads start, for all subprocesses (including git's `gh`
//! credential helper), not just the GitHub client's direct CLI invocations.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SHELL_TIMEOUT: Duration = Duration::from_secs(2);
const PATH_MARKER: &[u8] = b"\0GANDER_PATH\0";
const PRINT_PATH: &str = "printf '\\000GANDER_PATH\\000%s\\000' \"$PATH\"";
// Homebrew on both architectures, MacPorts, then the system tools when PATH
// was absent altogether. Append only: a terminal's explicit choices still win.
const FALLBACK_PATHS: &[&str] = &[
    "/opt/homebrew/bin",
    "/opt/homebrew/sbin",
    "/usr/local/bin",
    "/usr/local/sbin",
    "/opt/local/bin",
    "/opt/local/sbin",
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
];

/// Must be called at the start of main, before any threads or app subprocesses.
/// Only PATH is imported: shell startup must not override tokens or the app's
/// noninteractive git configuration.
pub fn init() {
    let inherited = std::env::var_os("PATH");
    let shell = std::env::var_os("SHELL")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| PathBuf::from("/bin/zsh"));
    // Interactive + login covers .zprofile AND .zshrc (or bash equivalents),
    // where package managers and custom gh installations usually set PATH.
    let recovered = shell_path(&shell, SHELL_TIMEOUT);
    if let Ok(path) = merged_path(inherited.as_deref(), recovered.as_deref()) {
        std::env::set_var("PATH", path);
    }
}

fn merged_path(
    inherited: Option<&OsStr>,
    recovered: Option<&OsStr>,
) -> Result<OsString, std::env::JoinPathsError> {
    let mut paths: Vec<PathBuf> = inherited
        .map(|path| std::env::split_paths(path).collect())
        .unwrap_or_default();
    let additions = recovered
        .into_iter()
        .flat_map(std::env::split_paths)
        .chain(FALLBACK_PATHS.iter().map(PathBuf::from));
    for path in additions {
        // Don't introduce a current-directory lookup from shell startup. An
        // empty entry explicitly inherited from the caller is left intact.
        if !path.as_os_str().is_empty() && !paths.contains(&path) {
            paths.push(path);
        }
    }
    std::env::join_paths(paths)
}

fn shell_path(shell: &Path, timeout: Duration) -> Option<OsString> {
    let mut command = Command::new(shell);
    command.args(["-ilc", PRINT_PATH]);
    let output = bounded_output(&mut command, timeout)?;
    parse_shell_path(&output)
}

fn parse_shell_path(output: &[u8]) -> Option<OsString> {
    // Shell startup may print banners; NUL framing keeps those out of PATH,
    // without making spaces, newlines or non-UTF-8 path names lossy.
    let start = output
        .windows(PATH_MARKER.len())
        .rposition(|window| window == PATH_MARKER)?
        + PATH_MARKER.len();
    let end = output[start..].iter().position(|byte| *byte == 0)? + start;
    (end > start).then(|| OsString::from_vec(output[start..end].to_vec()))
}

fn bounded_output(command: &mut Command, timeout: Duration) -> Option<Vec<u8>> {
    // A file avoids pipe backpressure from noisy startup scripts, and avoids
    // hanging while draining a pipe inherited by a background shell child.
    let mut output = tempfile::tempfile().ok()?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone().ok()?))
        .stderr(Stdio::null())
        // A stalled startup script may be waiting on a child (e.g. `sleep`).
        // Give it its own process group so timeout kills the whole job.
        .process_group(0)
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                break;
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                // SAFETY: this child was started in its own process group;
                // its positive PID is also that group's ID.
                unsafe {
                    libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    // Ignore pathological startup output instead of loading it into memory.
    const MAX_OUTPUT: u64 = 64 * 1024;
    if output.metadata().ok()?.len() > MAX_OUTPUT {
        return None;
    }
    output.seek(SeekFrom::Start(0)).ok()?;
    let mut bytes = Vec::new();
    output.take(MAX_OUTPUT + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() <= MAX_OUTPUT as usize).then_some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn keeps_inherited_precedence_then_shell_then_package_managers() {
        let path = merged_path(
            Some(OsStr::new("/custom/bin:/usr/bin")),
            Some(OsStr::new("/shell/bin:/usr/bin:/custom/bin:/shell/bin")),
        )
        .unwrap();
        let paths: Vec<_> = std::env::split_paths(&path).collect();
        assert_eq!(
            &paths[..4],
            &[
                PathBuf::from("/custom/bin"),
                PathBuf::from("/usr/bin"),
                PathBuf::from("/shell/bin"),
                PathBuf::from("/opt/homebrew/bin"),
            ]
        );
        assert_eq!(
            paths
                .iter()
                .filter(|p| **p == Path::new("/usr/bin"))
                .count(),
            1
        );
        assert!(paths.contains(&PathBuf::from("/usr/local/bin")));
        assert!(paths.contains(&PathBuf::from("/opt/local/bin")));
    }

    #[test]
    fn falls_back_when_path_or_shell_is_unavailable() {
        for inherited in [None, Some(OsStr::new("/usr/bin:/bin"))] {
            let path = merged_path(inherited, None).unwrap();
            let paths: Vec<_> = std::env::split_paths(&path).collect();
            for expected in FALLBACK_PATHS {
                assert!(paths.contains(&PathBuf::from(expected)));
            }
            assert!(!paths.contains(&PathBuf::new()));
        }
        assert!(shell_path(Path::new("/nonexistent/gander-shell"), SHELL_TIMEOUT).is_none());
    }

    #[test]
    fn does_not_add_empty_shell_path_entries() {
        let path =
            merged_path(Some(OsStr::new("/usr/bin")), Some(OsStr::new(":/custom::"))).unwrap();
        assert!(!std::env::split_paths(&path).any(|p| p.as_os_str().is_empty()));
        let path = merged_path(Some(OsStr::new(":/usr/bin")), None).unwrap();
        assert_eq!(std::env::split_paths(&path).next(), Some(PathBuf::new()));
    }

    #[test]
    fn parses_framed_path_despite_banners_and_non_utf8() {
        let path =
            parse_shell_path(b"Welcome!\n\0GANDER_PATH\0/a space:/non-utf8-\xff\0bye\n").unwrap();
        assert_eq!(path.as_bytes(), b"/a space:/non-utf8-\xff");
        assert!(parse_shell_path(b"a banner without a path").is_none());
        assert!(parse_shell_path(b"\0GANDER_PATH\0/truncated").is_none());
        assert!(parse_shell_path(b"\0GANDER_PATH\0\0").is_none());
        let merged = merged_path(None, Some(&path)).unwrap();
        assert_eq!(
            std::env::split_paths(&merged).next(),
            Some(PathBuf::from("/a space"))
        );
    }

    #[test]
    fn captures_shell_path_without_mutating_parent_environment() {
        let before = std::env::var_os("PATH");
        let mut command = Command::new("/bin/sh");
        command.args(["-c", &format!("printf 'a banner\\n'; {PRINT_PATH}")]);
        command.env("PATH", "/a custom/bin:/usr/bin");
        let output = bounded_output(&mut command, SHELL_TIMEOUT).unwrap();
        assert_eq!(
            parse_shell_path(&output),
            Some(OsString::from("/a custom/bin:/usr/bin"))
        );
        assert_eq!(std::env::var_os("PATH"), before);
    }

    #[test]
    fn finder_path_finds_custom_gh_for_direct_calls_and_helpers() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join("custom tools");
        std::fs::create_dir(&bin).unwrap();
        let gh = bin.join("gh");
        std::fs::write(&gh, "#!/bin/sh\nprintf 'fake gh: %s\\n' \"$*\"\n").unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(
            home.path().join(".zprofile"),
            "export GANDER_TEST_LOGIN=loaded\n",
        )
        .unwrap();
        std::fs::write(
            home.path().join(".zshrc"),
            "[[ $GANDER_TEST_LOGIN = loaded ]] || exit 1\n\
             export PATH=\"$HOME/custom tools:$PATH\"\n\
             printf 'startup banner\\n'\n",
        )
        .unwrap();

        // Like Finder: no Homebrew or custom paths inherited. Use an isolated
        // real login shell to verify both profile and interactive rc loading.
        let mut shell = Command::new("/bin/zsh");
        shell.args(["-ilc", PRINT_PATH]);
        shell.env_clear();
        shell.env("HOME", home.path());
        shell.env("ZDOTDIR", home.path());
        shell.env("PATH", "/usr/bin:/bin");
        let output = bounded_output(&mut shell, SHELL_TIMEOUT).unwrap();
        let recovered = parse_shell_path(&output).unwrap();
        let path = merged_path(Some(OsStr::new("/usr/bin:/bin")), Some(&recovered)).unwrap();

        let direct = Command::new("gh")
            .args(["api", "user"])
            .env("PATH", &path)
            .output()
            .unwrap();
        assert!(direct.status.success());
        assert_eq!(direct.stdout, b"fake gh: api user\n");
        // Git's !gh credential helper is invoked through a shell rather than
        // the Rust GitHub backend, and must see the same repaired PATH.
        let helper = Command::new("/bin/sh")
            .args(["-c", "gh auth git-credential"])
            .env("PATH", &path)
            .output()
            .unwrap();
        assert!(helper.status.success());
        assert_eq!(helper.stdout, b"fake gh: auth git-credential\n");
    }

    #[test]
    fn ignores_failed_and_stalled_shells() {
        let mut failed = Command::new("/bin/sh");
        failed.args(["-c", &format!("{PRINT_PATH}; exit 1")]);
        assert!(bounded_output(&mut failed, SHELL_TIMEOUT).is_none());

        let mut stalled = Command::new("/bin/sh");
        stalled.args(["-c", "/bin/sleep 30"]);
        let start = Instant::now();
        assert!(bounded_output(&mut stalled, Duration::from_millis(50)).is_none());
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
