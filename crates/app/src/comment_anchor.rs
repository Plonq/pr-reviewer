//! Pure mapping from selected source lines to GitHub anchors. Comments inside
//! one original patch hunk use the stable API; others use blob positioning.

use diff_core::{DiffRow, FileDiff, Hunk};
use github::{CommentAnchor, CommentPosition, DiffSide};

pub(crate) fn selection_anchor(
    file: &FileDiff,
    commit_id: &str,
    base_commit_id: &str,
    hunks: &[Hunk],
    points: &[(DiffSide, u64)],
) -> Result<CommentAnchor, String> {
    let &(side, _) = points
        .first()
        .ok_or("select a source line, not a header or comment")?;
    if points.iter().any(|(other, _)| *other != side) {
        return Err("a comment range must stay on the same side".into());
    }
    let start = points.iter().map(|(_, line)| *line).min().unwrap();
    let end = points.iter().map(|(_, line)| *line).max().unwrap();
    if points
        .iter()
        .any(|(_, line)| *line == 0 || *line > i32::MAX as u64)
    {
        return Err("invalid comment anchor".into());
    }
    let contains = |hunk: &Hunk, line: u64| {
        let (base, count) = match side {
            DiffSide::Left => (hunk.old_start, hunk.old_count),
            DiffSide::Right => (hunk.new_start, hunk.new_count),
        };
        line > 0
            && line >= u64::from(base)
            && line < u64::from(base) + u64::from(count)
            && hunk.rows.iter().any(|row| match (row, side) {
                (
                    DiffRow::Context { old_no, .. } | DiffRow::Removed { old_no, .. },
                    DiffSide::Left,
                ) => u64::from(*old_no) == line,
                (
                    DiffRow::Context { new_no, .. } | DiffRow::Added { new_no, .. },
                    DiffSide::Right,
                ) => u64::from(*new_no) == line,
                _ => false,
            })
    };
    let in_hunk = hunks
        .iter()
        .any(|hunk| points.iter().all(|(_, line)| contains(hunk, *line)));
    let position = if in_hunk {
        None
    } else {
        let (path, source_commit, base) = match side {
            DiffSide::Left => (
                file.old_path.as_deref(),
                base_commit_id,
                Some(base_commit_id.into()),
            ),
            DiffSide::Right => (file.new_path.as_deref(), commit_id, None),
        };
        Some(CommentPosition {
            path: path.ok_or("this side has no source file")?.into(),
            commit_id: source_commit.into(),
            base_commit_id: base,
        })
    };
    let anchor = CommentAnchor {
        path: file.display_path().into(),
        commit_id: commit_id.into(),
        side,
        line: end,
        start_line: (start != end).then_some(start),
        position,
    };
    anchor.validate().map_err(|err| err.to_string())?;
    Ok(anchor)
}

/// Missing current positions are outdated. Never attach an old position to the
/// current diff, even when its original line number happens to still exist.
pub(crate) fn display_line(line: Option<u64>, start: Option<u64>) -> Option<(Option<u64>, u64)> {
    let line = line.filter(|line| *line > 0 && *line <= i32::MAX as u64)?;
    Some((start.filter(|start| *start > 0 && *start < line), line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use github::DiffSide::{Left, Right};

    fn file() -> FileDiff {
        diff_core::parse_patch("diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -3,4 +5,4 @@\n a\n-b\n-c\n+B\n+C\n d\n@@ -12 +14 @@\n-x\n+y\n").files.remove(0)
    }

    fn anchor(points: &[(DiffSide, u64)]) -> Result<CommentAnchor, String> {
        let file = file();
        selection_anchor(&file, "sha", "base", &file.hunks, points)
    }

    #[test]
    fn single_adjacent_spanning_reversed_and_both_sides() {
        for (side, start) in [(Left, 3), (Right, 5)] {
            for width in 0..4 {
                let mut points = vec![(side, start), (side, start + width)];
                for _ in 0..2 {
                    let anchor = anchor(&points).unwrap();
                    assert_eq!(anchor.line, start + width);
                    assert_eq!(anchor.start_line, (width > 0).then_some(start));
                    assert_eq!(anchor.side, side);
                    assert_eq!(anchor.path, "a");
                    assert_eq!(anchor.commit_id, "sha");
                    assert_eq!(anchor.position, None);
                    points.reverse();
                }
            }
        }
    }

    #[test]
    fn invalid_selections_never_produce_anchors() {
        for points in [
            vec![],
            vec![(Right, 0)],
            vec![(Right, 5), (Left, 4)],
            vec![(Right, u64::MAX)],
        ] {
            assert!(anchor(&points).is_err(), "{points:?}");
        }
        let file = file();
        assert!(selection_anchor(&file, "", "base", &file.hunks, &[(Right, 5)]).is_err());
        assert!(selection_anchor(&file, "sha", "", &[], &[(Left, 1)]).is_err());
    }

    #[test]
    fn boundaries_are_inclusive_but_never_clamped_or_overflowed() {
        let file = file();
        let mut hunks = file.hunks.clone();
        hunks[0].new_count = 1;
        assert!(
            selection_anchor(&file, "sha", "base", &hunks, &[(Right, 5)])
                .unwrap()
                .position
                .is_none()
        );
        assert!(
            selection_anchor(&file, "sha", "base", &hunks, &[(Right, 6)])
                .unwrap()
                .position
                .is_some()
        );
        hunks[0].new_start = u32::MAX;
        hunks[0].new_count = u32::MAX;
        hunks[0].rows = vec![DiffRow::Added {
            new_no: u32::MAX,
            text: String::new(),
            intra: vec![],
        }];
        assert!(
            selection_anchor(&file, "sha", "base", &hunks, &[(Right, u32::MAX as u64)]).is_err()
        );
    }

    #[test]
    fn outside_hunks_and_cross_hunk_ranges_use_exact_blob_positions() {
        for (side, points) in [
            (Right, vec![(Right, 1)]),
            (Right, vec![(Right, 9)]),
            (Right, vec![(Right, 14), (Right, 5)]),
            (Left, vec![(Left, 3), (Left, 12)]),
        ] {
            let anchor = anchor(&points).unwrap();
            let position = anchor.position.unwrap();
            assert_eq!(position.path, "a");
            assert_eq!(
                position.commit_id,
                if side == Left { "base" } else { "sha" }
            );
            assert_eq!(
                position.base_commit_id,
                (side == Left).then(|| "base".into())
            );
            assert_eq!(
                anchor.line,
                points.iter().map(|(_, line)| *line).max().unwrap()
            );
        }
        let mut file = file();
        file.old_path = Some("old-name".into());
        let anchor = selection_anchor(&file, "sha", "base", &[], &[(Left, 1)]).unwrap();
        assert_eq!(anchor.path, "a");
        assert_eq!(anchor.position.unwrap().path, "old-name");
        assert!(selection_anchor(&file, "sha", "base", &[], &[(Right, 1)])
            .unwrap()
            .position
            .is_some());
    }

    #[test]
    fn outdated_and_malformed_display_positions_are_not_current_anchors() {
        assert_eq!(display_line(None, Some(10)), None);
        assert_eq!(display_line(Some(0), None), None);
        assert_eq!(display_line(Some(u64::MAX), None), None);
        assert_eq!(display_line(Some(10), Some(4)), Some((Some(4), 10)));
        for start in [None, Some(0), Some(10), Some(20)] {
            assert_eq!(display_line(Some(10), start), Some((None, 10)));
        }
    }
}
