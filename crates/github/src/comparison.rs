//! Upstream-aware review ranges. Compare the current head with the reviewed
//! tree after merging in the upstream history already present at that head.
//! The merge prefers the reviewed side on content conflicts: new PR resolutions
//! must remain visible. Nothing is checked out, committed, pushed, or modified
//! in the user's repository.

use crate::{ClientError, PrLocator};
use diff_core::{diff_texts, try_parse_patch, DiffRow};
use std::{collections::HashMap, path::Path, process::Command};

#[derive(Debug, Clone)]
pub struct ComparisonBase {
    pub text: String,
    /// Virtual baseline line -> line in the actual submitted-review blob.
    /// Upstream-inserted/replaced lines have no submitted-review position.
    pub reviewed_lines: Vec<Option<u32>>,
}

impl ComparisonBase {
    /// A comment range must map entirely and consecutively to the real blob.
    pub fn reviewed_range(&self, start: u64, end: u64) -> Option<(u64, u64)> {
        let start = usize::try_from(start.checked_sub(1)?).ok()?;
        let end = usize::try_from(end).ok()?;
        let lines = self.reviewed_lines.get(start..end)?;
        let first = (*lines.first()?)?;
        lines
            .iter()
            .enumerate()
            .all(|(i, line)| *line == first.checked_add(u32::try_from(i).unwrap_or(u32::MAX)))
            .then_some((u64::from(first), u64::from(first) + lines.len() as u64 - 1))
    }
}

#[derive(Debug, Clone, Default)]
pub struct ReviewComparison {
    pub patch: String,
    /// None: the actual reviewed tree is the baseline. Some: a virtual tree;
    /// missing entries must stay patch-derived, never fetch the old raw blob.
    pub base_files: Option<HashMap<String, ComparisonBase>>,
}

impl From<String> for ReviewComparison {
    fn from(patch: String) -> Self {
        Self {
            patch,
            base_files: None,
        }
    }
}
impl From<&str> for ReviewComparison {
    fn from(patch: &str) -> Self {
        patch.to_string().into()
    }
}

/// Fetch only the four immutable snapshots, with blobs fetched lazily by Git.
/// No branch checkout, PR commit enumeration, or capped compare file list.
/// `gh` supplies credentials via its standard Git credential helper; no token
/// is read by this process or persisted in the temporary repository.
pub(crate) fn fetch_comparison(
    loc: &PrLocator,
    reviewed: &str,
    head: &str,
    old_upstream: &str,
    new_upstream: &str,
) -> Result<ReviewComparison, ClientError> {
    let dir = tempfile::tempdir().map_err(failed)?;
    let repo = dir.path();
    git(repo, &["init", "--bare", "--quiet", "."])?;
    for (key, value) in [
        ("credential.helper", ""),
        ("credential.helper", "!gh auth git-credential"),
    ] {
        git(repo, &["config", "--add", key, value])?;
    }
    git(
        repo,
        &[
            "config",
            "remote.origin.url",
            &format!("https://github.com/{}.git", loc.repo_slug()),
        ],
    )?;
    git(repo, &["config", "remote.origin.promisor", "true"])?;
    git(
        repo,
        &["config", "remote.origin.partialclonefilter", "blob:none"],
    )?;
    git(
        repo,
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            "--depth=1",
            "--filter=blob:none",
            "origin",
            reviewed,
            head,
            old_upstream,
            new_upstream,
        ],
    )?;
    compare_trees(repo, reviewed, head, old_upstream, new_upstream)
}

fn compare_trees(
    repo: &Path,
    reviewed: &str,
    head: &str,
    old_upstream: &str,
    new_upstream: &str,
) -> Result<ReviewComparison, ClientError> {
    // Explicit base is essential for depth-one snapshots. Using the target
    // branch's tip instead of the head's merge base would include upstream
    // work the PR hasn't incorporated yet.
    let merged = git(
        repo,
        &[
            "merge-tree",
            "--write-tree",
            &format!("--merge-base={old_upstream}"),
            "-Xours",
            reviewed,
            new_upstream,
        ],
    )?;
    let tree = merged
        .lines()
        .next()
        .filter(|oid| crate::review_changes::valid_oid(oid))
        .ok_or_else(|| failed("Git returned no upstream-adjusted tree"))?;
    let patch = git(
        repo,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--find-renames",
            tree,
            head,
            "--",
        ],
    )?;
    let mut base_files = HashMap::new();
    for file in try_parse_patch(&patch).map_err(failed)?.files {
        let Some(path) = file.old_path else { continue };
        let Some(text) = text_blob(repo, tree, &path)? else {
            continue;
        };
        // A path introduced/renamed by upstream has no position at this path
        // in the reviewed commit. Right-side commenting is still available.
        let original = text_blob(repo, reviewed, &path)?.unwrap_or_default();
        let reviewed_lines = reviewed_lines(&original, &text);
        base_files.insert(
            path,
            ComparisonBase {
                text,
                reviewed_lines,
            },
        );
    }
    Ok(ReviewComparison {
        patch,
        base_files: Some(base_files),
    })
}

fn reviewed_lines(original: &str, virtual_text: &str) -> Vec<Option<u32>> {
    let mut lines = vec![None; virtual_text.lines().count()];
    if original == virtual_text {
        return (1..=lines.len()).map(|line| Some(line as u32)).collect();
    }
    let context = original.lines().count().max(lines.len()) as u32;
    for row in diff_texts(original, virtual_text, context)
        .into_iter()
        .flat_map(|h| h.rows)
    {
        if let DiffRow::Context { old_no, new_no, .. } = row {
            lines[new_no as usize - 1] = Some(old_no);
        }
    }
    lines
}

fn text_blob(repo: &Path, revision: &str, path: &str) -> Result<Option<String>, ClientError> {
    let object = format!("{revision}:{path}");
    // Gitlinks point at commits, not blobs. They stay patch-derived.
    let kind = command(repo)
        .args(["cat-file", "-t", &object])
        .output()
        .map_err(failed)?;
    if kind.status.success() && kind.stdout != b"blob\n" {
        return Ok(None);
    }
    // Don't materialize monster/binary files for full-content upgrades.
    let size = command(repo)
        .args(["cat-file", "-s", &object])
        .output()
        .map_err(failed)?;
    if !size.status.success() {
        // Distinguish a missing path from an object-fetch/authentication error.
        git(repo, &["cat-file", "-e", revision])?;
        let exists = git(
            repo,
            &[
                "ls-tree",
                "-z",
                revision,
                "--",
                &format!(":(literal){path}"),
            ],
        )?;
        return if exists.is_empty() {
            Ok(None)
        } else {
            Err(failed(String::from_utf8_lossy(&size.stderr)))
        };
    }
    let size = String::from_utf8_lossy(&size.stdout)
        .trim()
        .parse::<usize>()
        .map_err(failed)?;
    if size > 1024 * 1024 {
        return Ok(None);
    }
    let out = command(repo)
        .args(["cat-file", "blob", &object])
        .output()
        .map_err(failed)?;
    if !out.status.success() {
        return Err(failed(String::from_utf8_lossy(&out.stderr)));
    }
    Ok(String::from_utf8(out.stdout)
        .ok()
        .filter(|s| !s.contains('\0')))
}

fn command(repo: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_COUNT", "0")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_ASKPASS", "/usr/bin/false")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_DIR")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_NAMESPACE")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.quotePath=true",
        ]);
    cmd
}

fn git(repo: &Path, args: &[&str]) -> Result<String, ClientError> {
    let out = command(repo).args(args).output().map_err(failed)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(failed(if stderr.trim().is_empty() {
            format!(
                "git {} failed ({}); the reviewed tree could not be adjusted safely",
                args[0], out.status
            )
        } else {
            stderr.into_owned()
        }));
    }
    String::from_utf8(out.stdout).map_err(failed)
}

fn failed(error: impl std::fmt::Display) -> ClientError {
    ClientError::Failed(format!(
        "upstream-aware comparison (requires Git 2.43+): {error}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use diff_core::parse_patch;

    fn commit(repo: &Path, files: &[(&str, &str)]) -> String {
        for (path, text) in files {
            std::fs::write(repo.join(path), text).unwrap();
        }
        git(repo, &["add", "-A"]).unwrap();
        git(
            repo,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "snapshot",
            ],
        )
        .unwrap();
        git(repo, &["rev-parse", "HEAD"]).unwrap().trim().into()
    }

    #[test]
    fn excludes_upstream_files_and_edits_but_keeps_pr_reversions_and_resolutions() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git(repo, &["init", "-q"]).unwrap();
        let base = commit(
            repo,
            &[
                ("shared", "upstream-old\n\npr-old\n"),
                ("reverted", "base\n"),
                ("conflict", "base\n"),
            ],
        );
        let reviewed = commit(
            repo,
            &[
                ("shared", "upstream-old\n\npr-reviewed\n"),
                ("reverted", "reviewed\n"),
                ("conflict", "reviewed\n"),
            ],
        );
        git(repo, &["checkout", "--detach", &base]).unwrap();
        let upstream = commit(
            repo,
            &[
                ("shared", "upstream-new\ninserted\n\npr-old\n"),
                ("upstream-only", "unrelated\n"),
                ("conflict", "upstream\n"),
            ],
        );
        git(repo, &["checkout", "--detach", &reviewed]).unwrap();
        // A merge with a deliberate conflict resolution, followed by PR work.
        let _ = git(
            repo,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "merge",
                "--no-commit",
                &upstream,
            ],
        );
        let head = commit(
            repo,
            &[
                ("shared", "upstream-new\ninserted\n\npr-current\n"),
                ("reverted", "base\n"),
                ("conflict", "resolution\n"),
            ],
        );
        let comparison = compare_trees(repo, &reviewed, &head, &base, &upstream).unwrap();
        let diff = parse_patch(&comparison.patch);
        let paths: Vec<_> = diff.files.iter().map(|f| f.display_path()).collect();
        assert_eq!(paths, ["conflict", "reverted", "shared"]);
        assert!(diff
            .files
            .iter()
            .all(|f| (f.additions, f.deletions) == (1, 1)));
        let files = comparison.base_files.unwrap();
        assert_eq!(
            files["shared"].text,
            "upstream-new\ninserted\n\npr-reviewed\n"
        );
        assert_eq!(files["shared"].reviewed_range(4, 4), Some((3, 3)));
        assert_eq!(files["shared"].reviewed_range(1, 2), None);
        assert_eq!(files["conflict"].text, "reviewed\n");
    }

    #[test]
    fn upstream_only_is_empty_then_preserves_renames_deletions_and_binary_changes() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git(repo, &["init", "-q"]).unwrap();
        std::fs::write(repo.join("binary"), b"old\0bytes").unwrap();
        let base = commit(repo, &[("old-name", "base\n"), ("deleted", "base\n")]);
        let reviewed = commit(
            repo,
            &[
                ("old-name", "reviewed\n"),
                ("deleted", "deleted reviewed\n"),
            ],
        );
        git(repo, &["checkout", "--detach", &base]).unwrap();
        let upstream = commit(repo, &[("upstream-only", "not a PR edit\n")]);
        git(repo, &["checkout", "--detach", &reviewed]).unwrap();
        git(
            repo,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "merge",
                "--no-commit",
                &upstream,
            ],
        )
        .unwrap();
        let merged = commit(repo, &[]);
        let empty = compare_trees(repo, &reviewed, &merged, &base, &upstream).unwrap();
        assert!(empty.patch.is_empty());
        std::fs::rename(repo.join("old-name"), repo.join("new-name")).unwrap();
        std::fs::remove_file(repo.join("deleted")).unwrap();
        std::fs::write(repo.join("binary"), b"new\0bytes").unwrap();
        let head = commit(repo, &[("added", "new PR file\n")]);
        let comparison = compare_trees(repo, &reviewed, &head, &base, &upstream).unwrap();
        let diff = parse_patch(&comparison.patch);
        let statuses: HashMap<_, _> = diff
            .files
            .iter()
            .map(|f| (f.display_path(), f.status))
            .collect();
        assert_eq!(statuses.len(), 4);
        assert_eq!(statuses["added"], diff_core::FileStatus::Added);
        assert_eq!(statuses["binary"], diff_core::FileStatus::Binary);
        assert_eq!(statuses["deleted"], diff_core::FileStatus::Deleted);
        assert_eq!(statuses["new-name"], diff_core::FileStatus::Renamed);
        let files = comparison.base_files.unwrap();
        assert_eq!(files["old-name"].text, "reviewed\n");
        assert_eq!(files["deleted"].text, "deleted reviewed\n");
        assert!(!files.contains_key("binary"));
        assert!(text_blob(repo, &head, "absent").unwrap().is_none());
    }

    #[test]
    #[cfg(unix)]
    fn unsupported_patch_path_returns_a_visible_comparison_error() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git(repo, &["init", "-q"]).unwrap();
        let base = commit(repo, &[("valid", "text\n")]);
        let blob = git(repo, &["rev-parse", &format!("{base}:valid")]).unwrap();
        // macOS cannot create this filename in the filesystem, but Git trees
        // can contain it and an external comparison can therefore encounter it.
        let output = command(repo)
            .args([
                "update-index",
                "--add",
                "--cacheinfo",
                "100644",
                blob.trim(),
            ])
            .arg(OsStr::from_bytes(b"invalid-\xff"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let head = git(repo, &["write-tree"]).unwrap();

        let error = compare_trees(repo, &base, head.trim(), &base, &base).unwrap_err();
        assert!(matches!(error, ClientError::Failed(_)), "{error:?}");
        let message = error.to_string();
        assert!(message.contains("upstream-aware comparison"), "{message}");
        assert!(
            message.contains("Unsupported non-UTF-8 Git path"),
            "{message}"
        );
        assert!(message.contains(r"invalid-\\377"), "{message}");
    }

    #[test]
    fn unresolved_tree_conflicts_are_not_silently_rendered_as_a_valid_comparison() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git(repo, &["init", "-q"]).unwrap();
        let base = commit(repo, &[("file", "base\n")]);
        let reviewed = commit(repo, &[("file", "PR edits\n")]);
        git(repo, &["checkout", "--detach", &base]).unwrap();
        std::fs::remove_file(repo.join("file")).unwrap();
        let upstream = commit(repo, &[]);
        assert!(compare_trees(repo, &reviewed, &reviewed, &base, &upstream).is_err());
    }

    #[test]
    fn maps_only_consecutive_real_review_lines() {
        let base = ComparisonBase {
            text: "a\nupstream\nb\nc\n".into(),
            reviewed_lines: reviewed_lines("a\nb\nc\n", "a\nupstream\nb\nc\n"),
        };
        assert_eq!(base.reviewed_range(3, 4), Some((2, 3)));
        for (a, b) in [(0, 1), (1, 3), (2, 2), (4, 3), (5, 5), (1, u64::MAX)] {
            assert_eq!(base.reviewed_range(a, b), None);
        }
    }
}
