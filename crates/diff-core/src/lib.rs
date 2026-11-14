//! UI-free diff model: parse `git diff` / `gh pr diff` unified patches into a
//! structured model, with word-level intra-line highlights on modified runs.

use imara_diff::intern::InternedInput;
use imara_diff::Algorithm;
use std::ops::Range;

#[derive(Debug, Default)]
pub struct PrDiff {
    pub files: Vec<FileDiff>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
    Binary,
}

#[derive(Debug)]
pub struct FileDiff {
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub status: FileStatus,
    pub hunks: Vec<Hunk>,
    pub additions: u32,
    pub deletions: u32,
}

impl FileDiff {
    pub fn display_path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or("<unknown>")
    }
}

#[derive(Debug, Clone)]
pub struct Hunk {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    /// Function/section context after the trailing `@@`, if any.
    pub section: String,
    pub rows: Vec<DiffRow>,
}

/// A row is only ever Context, Added, or Removed — "modified" exists only at
/// the hunk level, as removed rows followed by added rows.
#[derive(Debug, Clone)]
pub enum DiffRow {
    Context {
        old_no: u32,
        new_no: u32,
        text: String,
    },
    Added {
        new_no: u32,
        text: String,
        /// Byte ranges within `text` that differ from the paired removed line.
        intra: Vec<Range<usize>>,
    },
    Removed {
        old_no: u32,
        text: String,
        intra: Vec<Range<usize>>,
    },
}

/// Parse a patch whose paths are known to be supported. Call
/// [`try_parse_patch`] for external input so path errors can be shown to users.
pub fn parse_patch(patch: &str) -> PrDiff {
    try_parse_patch(patch).expect("invalid or unsupported Git patch path")
}

#[derive(Debug)]
pub struct PatchPathError(String);

impl std::fmt::Display for PatchPathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PatchPathError {}

/// Decode Git's quoted path bytes exactly once, rejecting paths the UTF-8
/// file model cannot represent rather than substituting a different filename.
pub fn try_parse_patch(patch: &str) -> Result<PrDiff, PatchPathError> {
    let mut files = Vec::new();
    let mut lines = patch.lines().peekable();

    while let Some(line) = lines.next() {
        let Some(rest) = line.strip_prefix("diff --git ") else {
            continue;
        };
        let (old_guess, new_guess) = parse_git_paths(rest)?;
        let mut file = FileDiff {
            old_path: old_guess,
            new_path: new_guess,
            status: FileStatus::Modified,
            hunks: Vec::new(),
            additions: 0,
            deletions: 0,
        };
        let mut is_rename = false;
        let mut is_added = false;
        let mut is_deleted = false;

        // Extended header lines, up to the first hunk or the next file.
        while let Some(next) = lines.peek() {
            if next.starts_with("diff --git ") || next.starts_with("@@ ") {
                break;
            }
            let next = lines.next().unwrap();
            if next.starts_with("new file mode") {
                file.status = FileStatus::Added;
                is_added = true;
            } else if next.starts_with("deleted file mode") {
                file.status = FileStatus::Deleted;
                is_deleted = true;
            } else if let Some(p) = next.strip_prefix("rename from ") {
                file.old_path = Some(decode_path(p)?);
                is_rename = true;
            } else if let Some(p) = next.strip_prefix("rename to ") {
                file.new_path = Some(decode_path(p)?);
                is_rename = true;
            } else if next.starts_with("Binary files ") || next == "GIT binary patch" {
                file.status = FileStatus::Binary;
            } else if let Some(p) = next.strip_prefix("--- ") {
                if let Some(p) = parse_marker_path(p)? {
                    file.old_path = Some(p);
                }
            } else if let Some(p) = next.strip_prefix("+++ ") {
                if let Some(p) = parse_marker_path(p)? {
                    file.new_path = Some(p);
                }
            }
            // index/mode/similarity lines are ignored.
        }
        if is_rename && file.status == FileStatus::Modified {
            file.status = FileStatus::Renamed;
        }
        if is_added {
            file.old_path = None;
        }
        if is_deleted {
            file.new_path = None;
        }

        while let Some(next) = lines.peek() {
            if !next.starts_with("@@ ") {
                break;
            }
            let header = lines.next().unwrap();
            let Some(mut hunk) = parse_hunk_header(header) else {
                break;
            };
            let mut old_no = hunk.old_start;
            let mut new_no = hunk.new_start;
            while let Some(body) = lines.peek() {
                if body.starts_with("diff --git ") || body.starts_with("@@ ") {
                    break;
                }
                let body = lines.next().unwrap();
                if let Some(text) = body.strip_prefix('+') {
                    hunk.rows.push(DiffRow::Added {
                        new_no,
                        text: text.to_string(),
                        intra: Vec::new(),
                    });
                    new_no += 1;
                    file.additions += 1;
                } else if let Some(text) = body.strip_prefix('-') {
                    hunk.rows.push(DiffRow::Removed {
                        old_no,
                        text: text.to_string(),
                        intra: Vec::new(),
                    });
                    old_no += 1;
                    file.deletions += 1;
                } else if body.starts_with('\\') {
                    // "\ No newline at end of file"
                } else {
                    // Context. Tolerate a bare empty line (some tools strip the
                    // leading space from blank context lines).
                    let text = body.strip_prefix(' ').unwrap_or(body);
                    hunk.rows.push(DiffRow::Context {
                        old_no,
                        new_no,
                        text: text.to_string(),
                    });
                    old_no += 1;
                    new_no += 1;
                }
            }
            compute_intra_line(&mut hunk.rows);
            file.hunks.push(hunk);
        }
        files.push(file);
    }

    Ok(PrDiff { files })
}

/// Authoritative diff of two complete file contents into hunks with `context`
/// lines of surrounding context, merging hunks whose context overlaps or
/// touches (git semantics). Line endings are normalized first: lines are
/// compared without their terminators (`str::lines`, which strips both `\n`
/// and `\r\n`), so a CRLF↔LF flip produces no hunks. Consequence, chosen and
/// documented: a difference only in the presence of a trailing newline is
/// invisible to this diff — both `"a\nb"` and `"a\nb\n"` are the lines
/// `["a", "b"]`. Row texts never include terminators.
pub fn diff_texts(old: &str, new: &str, context: u32) -> Vec<Hunk> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    let mut input = InternedInput::default();
    input.update_before(old_lines.iter().copied());
    input.update_after(new_lines.iter().copied());

    // Changed regions, as 0-based line ranges on each side.
    let mut changes: Vec<(Range<u32>, Range<u32>)> = Vec::new();
    imara_diff::diff(
        Algorithm::Histogram,
        &input,
        |before: Range<u32>, after: Range<u32>| changes.push((before, after)),
    );
    if changes.is_empty() {
        return Vec::new();
    }

    // Group changes into hunks: two changes share a hunk when the context
    // lines around them overlap or touch (gap between them ≤ 2 * context).
    let mut groups: Vec<Range<usize>> = Vec::new();
    let mut start = 0;
    for ix in 1..changes.len() {
        if changes[ix].0.start - changes[ix - 1].0.end > 2 * context {
            groups.push(start..ix);
            start = ix;
        }
    }
    groups.push(start..changes.len());

    let mut hunks = Vec::new();
    for group in groups {
        let first = &changes[group.start];
        let last = &changes[group.end - 1];
        // Context clamps at file boundaries.
        let old_lo = first.0.start.saturating_sub(context);
        let old_hi = (last.0.end + context).min(old_lines.len() as u32);
        let new_lo = first.1.start.saturating_sub(context);
        let new_hi = (last.1.end + context).min(new_lines.len() as u32);

        let mut rows = Vec::new();
        let (mut old_no, mut new_no) = (old_lo, new_lo); // 0-based cursors
        for (before, after) in &changes[group] {
            // Shared context up to this change (identical on both sides).
            while old_no < before.start {
                rows.push(DiffRow::Context {
                    old_no: old_no + 1,
                    new_no: new_no + 1,
                    text: old_lines[old_no as usize].to_string(),
                });
                old_no += 1;
                new_no += 1;
            }
            for no in before.clone() {
                rows.push(DiffRow::Removed {
                    old_no: no + 1,
                    text: old_lines[no as usize].to_string(),
                    intra: Vec::new(),
                });
            }
            for no in after.clone() {
                rows.push(DiffRow::Added {
                    new_no: no + 1,
                    text: new_lines[no as usize].to_string(),
                    intra: Vec::new(),
                });
            }
            old_no = before.end;
            new_no = after.end;
        }
        while old_no < old_hi {
            rows.push(DiffRow::Context {
                old_no: old_no + 1,
                new_no: new_no + 1,
                text: old_lines[old_no as usize].to_string(),
            });
            old_no += 1;
            new_no += 1;
        }
        compute_intra_line(&mut rows);

        let old_count = old_hi - old_lo;
        let new_count = new_hi - new_lo;
        hunks.push(Hunk {
            // Git convention: a zero-count side records the line *before* the
            // hunk (0 when at file start), otherwise the 1-based first line.
            old_start: if old_count == 0 { old_lo } else { old_lo + 1 },
            old_count,
            new_start: if new_count == 0 { new_lo } else { new_lo + 1 },
            new_count,
            section: String::new(),
            rows,
        });
    }
    hunks
}

/// Re-align each patch hunk ignoring whitespace, retaining real source line
/// numbers and new-side text. Works even before full blobs are available.
pub fn without_whitespace(hunks: &[Hunk]) -> Vec<Hunk> {
    hunks
        .iter()
        .map(|hunk| {
            let mut old = Vec::new();
            let mut new = Vec::new();
            for row in &hunk.rows {
                match row {
                    DiffRow::Context {
                        old_no,
                        new_no,
                        text,
                    } => {
                        old.push((*old_no, text));
                        new.push((*new_no, text));
                    }
                    DiffRow::Removed { old_no, text, .. } => old.push((*old_no, text)),
                    DiffRow::Added { new_no, text, .. } => new.push((*new_no, text)),
                }
            }
            let normalize = |text: &&String| {
                text.chars()
                    .filter(|ch| !ch.is_whitespace())
                    .collect::<String>()
            };
            let before: Vec<_> = old.iter().map(|(_, text)| normalize(text)).collect();
            let after: Vec<_> = new.iter().map(|(_, text)| normalize(text)).collect();
            let mut input = InternedInput::default();
            input.update_before(before.iter().map(String::as_str));
            input.update_after(after.iter().map(String::as_str));
            let mut changes = Vec::new();
            imara_diff::diff(
                Algorithm::Histogram,
                &input,
                |a: Range<u32>, b: Range<u32>| changes.push((a, b)),
            );
            let mut rows = Vec::new();
            let (mut a, mut b) = (0, 0);
            for (removed, added) in changes {
                while a < removed.start as usize {
                    rows.push(DiffRow::Context {
                        old_no: old[a].0,
                        new_no: new[b].0,
                        text: new[b].1.clone(),
                    });
                    a += 1;
                    b += 1;
                }
                for ix in removed.clone() {
                    rows.push(DiffRow::Removed {
                        old_no: old[ix as usize].0,
                        text: old[ix as usize].1.clone(),
                        intra: Vec::new(),
                    });
                }
                for ix in added.clone() {
                    rows.push(DiffRow::Added {
                        new_no: new[ix as usize].0,
                        text: new[ix as usize].1.clone(),
                        intra: Vec::new(),
                    });
                }
                a = removed.end as usize;
                b = added.end as usize;
            }
            while a < old.len() && b < new.len() {
                rows.push(DiffRow::Context {
                    old_no: old[a].0,
                    new_no: new[b].0,
                    text: new[b].1.clone(),
                });
                a += 1;
                b += 1;
            }
            compute_intra_line(&mut rows);
            Hunk {
                rows,
                ..hunk.clone()
            }
        })
        .collect()
}

/// `a/old b/new`, possibly with `"`-quoted paths. Best-effort: the definitive
/// paths come from `---`/`+++`/`rename` lines when present.
fn parse_git_paths(rest: &str) -> Result<(Option<String>, Option<String>), PatchPathError> {
    if rest.starts_with('"') {
        let (old, rest) = decode_quoted_path(rest)?;
        let new = decode_path(rest.strip_prefix(' ').ok_or_else(|| invalid_path(rest))?)?;
        return Ok((strip_side(&old), strip_side(&new)));
    }
    // Either side may be quoted independently (e.g. a renamed empty file).
    if let Some((old, new)) = rest.split_once(" \"") {
        return Ok((
            strip_side(old),
            strip_side(&decode_path(&format!("\"{new}"))?),
        ));
    }
    // Spaces, including " b/", are legal unquoted filename bytes. Prefer the
    // split where both names agree, which also handles header-only empty and
    // binary files. Rename headers/markers supply authoritative differing names.
    for (pos, _) in rest.match_indices(" b/") {
        if rest[..pos].strip_prefix("a/") == rest[pos + 1..].strip_prefix("b/") {
            return Ok((strip_side(&rest[..pos]), strip_side(&rest[pos + 1..])));
        }
    }
    if let Some(pos) = rest.find(" b/") {
        let old = strip_side(&rest[..pos]);
        let new = strip_side(&rest[pos + 1..]);
        return Ok((old, new));
    }
    Ok((None, None))
}

fn strip_side(path: &str) -> Option<String> {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .map(str::to_string)
}

/// Path from a `---`/`+++` marker: `a/path`, `b/path`, `/dev/null`, maybe quoted.
fn parse_marker_path(p: &str) -> Result<Option<String>, PatchPathError> {
    // Git adds a tab terminator to unquoted marker paths containing spaces.
    // Literal filename tabs are quoted; do not trim meaningful spaces.
    let p = decode_path(p.split_once('\t').map_or(p, |(path, _)| path))?;
    if p == "/dev/null" {
        return Ok(None);
    }
    Ok(strip_side(&p).or(Some(p)))
}

fn invalid_path(path: &str) -> PatchPathError {
    PatchPathError(format!("Invalid Git patch path: {path:?}"))
}

fn decode_path(path: &str) -> Result<String, PatchPathError> {
    if !path.starts_with('"') {
        return Ok(path.to_string());
    }
    let (decoded, rest) = decode_quoted_path(path)?;
    if !rest.is_empty() {
        return Err(invalid_path(path));
    }
    Ok(decoded)
}

fn decode_quoted_path(path: &str) -> Result<(String, &str), PatchPathError> {
    let input = path.as_bytes();
    let mut bytes = Vec::new();
    let mut ix = 1; // opening quote
    while let Some(&byte) = input.get(ix) {
        ix += 1;
        match byte {
            b'"' => {
                let decoded = String::from_utf8(bytes).map_err(|_| {
                    PatchPathError(format!("Unsupported non-UTF-8 Git path: {path:?}"))
                })?;
                if decoded.contains('\0') {
                    return Err(invalid_path(path));
                }
                return Ok((decoded, &path[ix..]));
            }
            b'\\' => {
                let escaped = *input.get(ix).ok_or_else(|| invalid_path(path))?;
                ix += 1;
                bytes.push(match escaped {
                    b'a' => 7,
                    b'b' => 8,
                    b't' => b'\t',
                    b'n' => b'\n',
                    b'v' => 11,
                    b'f' => 12,
                    b'r' => b'\r',
                    b'"' | b'\\' => escaped,
                    b'0'..=b'3' => {
                        let digits = input.get(ix..ix + 2).ok_or_else(|| invalid_path(path))?;
                        if !digits.iter().all(|d| matches!(d, b'0'..=b'7')) {
                            return Err(invalid_path(path));
                        }
                        ix += 2;
                        (escaped - b'0') * 64 + (digits[0] - b'0') * 8 + (digits[1] - b'0')
                    }
                    _ => return Err(invalid_path(path)),
                });
            }
            _ => bytes.push(byte),
        }
    }
    Err(invalid_path(path))
}

/// `@@ -old_start[,old_count] +new_start[,new_count] @@[ section]`
fn parse_hunk_header(line: &str) -> Option<Hunk> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, rest) = rest.split_once(" @@")?;
    let parse_pair = |s: &str| -> Option<(u32, u32)> {
        match s.split_once(',') {
            Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
            None => Some((s.parse().ok()?, 1)),
        }
    };
    let (old_start, old_count) = parse_pair(old)?;
    let (new_start, new_count) = parse_pair(new)?;
    Some(Hunk {
        old_start,
        old_count,
        new_start,
        new_count,
        section: rest.trim_start().to_string(),
        rows: Vec::new(),
    })
}

// --- Intra-line (word-level) diff -------------------------------------------

const MAX_BLOCK_LINES: usize = 32;
const MAX_LINE_BYTES: usize = 4096;
const MAX_BLOCK_TOKENS: usize = 512;
/// If more than this fraction of a block changed, highlighting it all is noise.
const MAX_CHANGED_FRACTION: f32 = 0.85;

/// Diff a whole replacement block so unchanged words can match across line
/// boundaries (including reflowed text and unequal numbers of lines).
fn compute_intra_line(rows: &mut [DiffRow]) {
    let mut i = 0;
    while i < rows.len() {
        if !matches!(rows[i], DiffRow::Removed { .. }) {
            i += 1;
            continue;
        }
        let start = i;
        while i < rows.len() && matches!(rows[i], DiffRow::Removed { .. }) {
            i += 1;
        }
        let mid = i;
        while i < rows.len() && matches!(rows[i], DiffRow::Added { .. }) {
            i += 1;
        }
        if mid == i || mid - start > MAX_BLOCK_LINES || i - mid > MAX_BLOCK_LINES {
            continue;
        }
        fn text(row: &DiffRow) -> &str {
            match row {
                DiffRow::Removed { text, .. } | DiffRow::Added { text, .. } => text.as_str(),
                _ => unreachable!(),
            }
        }
        if rows[start..i]
            .iter()
            .any(|row| text(row).len() > MAX_LINE_BYTES)
        {
            continue;
        }
        let old = rows[start..mid]
            .iter()
            .map(text)
            .collect::<Vec<_>>()
            .join("\n");
        let new = rows[mid..i].iter().map(text).collect::<Vec<_>>().join("\n");
        let (old_ranges, new_ranges) = word_diff(&old, &new);
        set_intra_ranges(&mut rows[start..mid], &old_ranges);
        set_intra_ranges(&mut rows[mid..i], &new_ranges);
    }
}

/// Project block byte offsets onto row text, excluding the joining newlines.
fn set_intra_ranges(rows: &mut [DiffRow], ranges: &[Range<usize>]) {
    let mut offset = 0;
    for row in rows {
        let (text, intra) = match row {
            DiffRow::Removed { text, intra, .. } | DiffRow::Added { text, intra, .. } => {
                (text, intra)
            }
            _ => unreachable!(),
        };
        *intra = ranges
            .iter()
            .filter_map(|range| {
                let start = range.start.max(offset);
                let end = range.end.min(offset + text.len());
                (start < end).then(|| start - offset..end - offset)
            })
            .collect();
        offset += text.len() + 1;
    }
}

/// Byte ranges of tokens: identifier/number runs, whitespace runs, or single
/// punctuation chars.
fn token_ranges(s: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut chars = s.char_indices().peekable();
    while let Some((start, ch)) = chars.next() {
        let class = char_class(ch);
        let mut end = start + ch.len_utf8();
        if class != CharClass::Punct {
            while let Some(&(next_ix, next_ch)) = chars.peek() {
                if char_class(next_ch) != class {
                    break;
                }
                end = next_ix + next_ch.len_utf8();
                chars.next();
            }
        }
        out.push(start..end);
    }
    out
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum CharClass {
    Word,
    Space,
    Punct,
}

fn char_class(ch: char) -> CharClass {
    if ch.is_alphanumeric() || ch == '_' {
        CharClass::Word
    } else if ch.is_whitespace() {
        CharClass::Space
    } else {
        CharClass::Punct
    }
}

/// Word-level diff of two replacement blocks → (changed byte ranges in old, in new).
/// Returns empty ranges when the blocks are too different for highlights to help.
fn word_diff(old: &str, new: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let old_tokens = token_ranges(old);
    let new_tokens = token_ranges(new);
    if old_tokens.len() > MAX_BLOCK_TOKENS || new_tokens.len() > MAX_BLOCK_TOKENS {
        return (Vec::new(), Vec::new());
    }

    let mut input = InternedInput::default();
    input.update_before(old_tokens.iter().map(|r| &old[r.clone()]));
    input.update_after(new_tokens.iter().map(|r| &new[r.clone()]));

    let mut old_ranges: Vec<Range<usize>> = Vec::new();
    let mut new_ranges: Vec<Range<usize>> = Vec::new();
    imara_diff::diff(
        Algorithm::Histogram,
        &input,
        |before: Range<u32>, after: Range<u32>| {
            if before.start < before.end {
                let start = old_tokens[before.start as usize].start;
                let end = old_tokens[before.end as usize - 1].end;
                old_ranges.push(start..end);
            }
            if after.start < after.end {
                let start = new_tokens[after.start as usize].start;
                let end = new_tokens[after.end as usize - 1].end;
                new_ranges.push(start..end);
            }
        },
    );

    let changed = |ranges: &[Range<usize>], len: usize| {
        len > 0
            && ranges.iter().map(|r| r.len()).sum::<usize>() as f32 / len as f32
                > MAX_CHANGED_FRACTION
    };
    if changed(&old_ranges, old.trim_end().len()) || changed(&new_ranges, new.trim_end().len()) {
        return (Vec::new(), Vec::new());
    }
    // Shared spaces between replaced words should not split a visual highlight.
    // Do this after the rewrite cutoff: these spaces are not actual edits.
    (
        merge_whitespace_gaps(old, old_ranges),
        merge_whitespace_gaps(new, new_ranges),
    )
}

fn merge_whitespace_gaps(text: &str, ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        if let Some(previous) = merged.last_mut() {
            if text[previous.end..range.start]
                .chars()
                .all(char::is_whitespace)
            {
                previous.end = range.end;
                continue;
            }
        }
        merged.push(range);
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_quoted_paths_decode_bytes_once_for_markers_and_renames() {
        let encoded = r#"\303\251\t\n\"\\literal\\n "#;
        let decoded = "é\t\n\"\\literal\\n ";
        let patch = format!(
            "diff --git \"a/{encoded}\" \"b/new-{encoded}\"\nsimilarity index 90%\nrename from \"{encoded}\"\nrename to \"new-{encoded}\"\n--- \"a/{encoded}\"\n+++ \"b/new-{encoded}\"\n@@ -1 +1 @@\n-old\n+new\n"
        );
        let diff = try_parse_patch(&patch).unwrap();
        let file = &diff.files[0];
        assert_eq!(file.old_path.as_deref(), Some(decoded));
        assert_eq!(
            file.new_path.as_deref(),
            Some(format!("new-{decoded}").as_str())
        );
        assert_eq!(file.status, FileStatus::Renamed);

        // A pure rename has no marker paths to repair its extended headers.
        let patch = format!(
            "diff --git a/plain \"b/{encoded}\"\nsimilarity index 100%\nrename from plain\nrename to \"{encoded}\"\n"
        );
        let diff = try_parse_patch(&patch).unwrap();
        assert_eq!(diff.files[0].old_path.as_deref(), Some("plain"));
        assert_eq!(diff.files[0].new_path.as_deref(), Some(decoded));

        let diff = try_parse_patch("diff --git \"a/has\\ttab\" b/plain\nsimilarity index 100%\nrename from \"has\\ttab\"\nrename to plain\n").unwrap();
        assert_eq!(diff.files[0].old_path.as_deref(), Some("has\ttab"));
        assert_eq!(diff.files[0].new_path.as_deref(), Some("plain"));
    }

    #[test]
    fn unquoted_paths_preserve_unicode_and_spaces_including_header_only_files() {
        for path in ["é世界", " name  ", "has b/inside "] {
            for suffix in [
                "new file mode 100644\nindex 0000000..e69de29\n".to_string(),
                format!(
                    "new file mode 100644\n--- /dev/null\n+++ b/{path}\t\n@@ -0,0 +1 @@\n+text\n"
                ),
            ] {
                let diff =
                    try_parse_patch(&format!("diff --git a/{path} b/{path}\n{suffix}")).unwrap();
                assert_eq!(diff.files[0].old_path, None);
                assert_eq!(diff.files[0].new_path.as_deref(), Some(path));
            }
        }
        let diff = try_parse_patch(
            "diff --git a/old  b/new  \nsimilarity index 100%\nrename from old \nrename to new  \n",
        )
        .unwrap();
        assert_eq!(diff.files[0].old_path.as_deref(), Some("old "));
        assert_eq!(diff.files[0].new_path.as_deref(), Some("new  "));
    }

    #[test]
    fn unsupported_or_invalid_quoted_paths_are_errors() {
        for patch in [
            "diff --git \"a/\\377\" \"b/\\377\"\n",
            "diff --git a/old b/new\nrename to \"\\377\"\n",
            "diff --git a/old b/new\n+++ \"b/\\377\"\n",
            "diff --git \"a/unterminated b/new\n",
            "diff --git a/old b/new\nrename from \"bad\\q\"\n",
            "diff --git a/old b/new\nrename from \"bad\\000\"\n",
        ] {
            assert!(try_parse_patch(patch).is_err(), "{patch:?}");
        }
    }

    #[test]
    fn binary_additions_and_deletions_keep_the_absent_side_absent() {
        let diff = parse_patch(concat!(
            "diff --git a/new.png b/new.png\nnew file mode 100644\nBinary files /dev/null and b/new.png differ\n",
            "diff --git a/old.png b/old.png\ndeleted file mode 100644\nBinary files a/old.png and /dev/null differ\n",
        ));
        assert_eq!(diff.files[0].status, FileStatus::Binary);
        assert_eq!(diff.files[0].old_path, None);
        assert_eq!(diff.files[0].new_path.as_deref(), Some("new.png"));
        assert_eq!(diff.files[1].status, FileStatus::Binary);
        assert_eq!(diff.files[1].old_path.as_deref(), Some("old.png"));
        assert_eq!(diff.files[1].new_path, None);
    }

    const SAMPLE: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 1111111..2222222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,4 +1,5 @@ fn main()
 use std::io;
-let foo = 1;
+let bar = 1;
+let baz = 2;
 println!();
@@ -10,2 +11,2 @@
 // tail
-old line
+new line
diff --git a/README.md b/README.md
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/README.md
@@ -0,0 +1,1 @@
+# hello
\\ No newline at end of file
diff --git a/logo.png b/logo.png
index 4444444..5555555 100644
Binary files a/logo.png and b/logo.png differ
diff --git a/old.rs b/renamed.rs
similarity index 90%
rename from old.rs
rename to renamed.rs
index 6666666..7777777 100644
--- a/old.rs
+++ b/renamed.rs
@@ -1,1 +1,1 @@
-x
+y
";

    #[test]
    fn whitespace_filter_realigns_runs_without_changing_source_coordinates() {
        let hunks = diff_texts(
            "a\n  same();\nold();\nz\n",
            "a\n\tsame ( );  \ninsert();\nnew();\nz\n",
            3,
        );
        let filtered = without_whitespace(&hunks);
        let h = &filtered[0];
        assert_eq!((h.old_count, h.new_count), (4, 5));
        assert!(
            matches!(&h.rows[1], DiffRow::Context { old_no: 2, new_no: 2, text } if text == "\tsame ( );  ")
        );
        assert!(h
            .rows
            .iter()
            .any(|r| matches!(r, DiffRow::Removed { old_no: 3, text, .. } if text == "old();")));
        assert!(h
            .rows
            .iter()
            .any(|r| matches!(r, DiffRow::Added { new_no: 4, text, .. } if text == "new();")));
        assert!(matches!(
            h.rows.last(),
            Some(DiffRow::Context {
                old_no: 4,
                new_no: 5,
                ..
            })
        ));
        assert!(
            matches!(&hunks[0].rows[1], DiffRow::Removed { .. }),
            "the source diff is untouched"
        );
    }

    #[test]
    fn whitespace_only_changes_become_context_but_insertions_and_deletions_survive() {
        let h = without_whitespace(&diff_texts("a b\nc\n", "ab\n c \n", 3));
        assert!(h[0]
            .rows
            .iter()
            .all(|r| matches!(r, DiffRow::Context { .. })));
        for (old, new) in [("", "added\n"), ("removed\n", "")] {
            let h = without_whitespace(&diff_texts(old, new, 3));
            assert_eq!(h[0].rows.len(), 1);
            assert!(!matches!(h[0].rows[0], DiffRow::Context { .. }));
        }
    }

    #[test]
    fn parses_files_hunks_and_rows() {
        let diff = parse_patch(SAMPLE);
        assert_eq!(diff.files.len(), 4);

        let main = &diff.files[0];
        assert_eq!(main.display_path(), "src/main.rs");
        assert_eq!(main.status, FileStatus::Modified);
        assert_eq!(main.hunks.len(), 2);
        assert_eq!((main.additions, main.deletions), (3, 2));
        assert_eq!(main.hunks[0].section, "fn main()");
        assert_eq!(main.hunks[0].rows.len(), 5);
        match &main.hunks[0].rows[1] {
            DiffRow::Removed { old_no, text, .. } => {
                assert_eq!(*old_no, 2);
                assert_eq!(text, "let foo = 1;");
            }
            other => panic!("expected removed row, got {other:?}"),
        }
        match &main.hunks[0].rows[4] {
            DiffRow::Context { old_no, new_no, .. } => assert_eq!((*old_no, *new_no), (3, 4)),
            other => panic!("expected context row, got {other:?}"),
        }

        let readme = &diff.files[1];
        assert_eq!(readme.status, FileStatus::Added);
        assert_eq!(readme.old_path, None);
        assert_eq!(readme.display_path(), "README.md");

        let logo = &diff.files[2];
        assert_eq!(logo.status, FileStatus::Binary);
        assert_eq!(logo.display_path(), "logo.png");
        assert!(logo.hunks.is_empty());

        let renamed = &diff.files[3];
        assert_eq!(renamed.status, FileStatus::Renamed);
        assert_eq!(renamed.old_path.as_deref(), Some("old.rs"));
        assert_eq!(renamed.new_path.as_deref(), Some("renamed.rs"));
    }

    fn highlighted(rows: &[DiffRow], added: bool) -> Vec<String> {
        rows.iter()
            .filter_map(|row| match row {
                DiffRow::Added { text, intra, .. } if added => Some((text, intra)),
                DiffRow::Removed { text, intra, .. } if !added => Some((text, intra)),
                _ => None,
            })
            .map(|(text, intra)| intra.iter().map(|r| &text[r.clone()]).collect::<String>())
            .collect()
    }

    #[test]
    fn block_diff_matches_words_across_reflowed_lines() {
        let hunks = diff_texts(
            "The old value brackets the wait itself.\nKeep this context.\n",
            "The new value brackets\nthe wait itself.\nKeep this context.\n",
            3,
        );
        assert_eq!(highlighted(&hunks[0].rows, false), vec!["old "]);
        assert_eq!(highlighted(&hunks[0].rows, true), vec!["new", ""]);
    }

    #[test]
    fn issue_51_keeps_the_shared_phrase_unhighlighted() {
        let old = "``assigned_at`` and ``ended_at`` both stay null, as they do on a\ncancel: they bracket a live chat, and a request nobody accepted never\nbecame one. ``created`` and ``modified`` bracket the wait itself.";
        let new = "``queued_at`` and ``modified`` bracket the wait itself — not\n``created``, which on a requeued chat predates a spell that was spent\nbeing handled rather than waiting.";
        let hunks = diff_texts(old, new, 0);
        let (old_ranges, new_ranges) = word_diff(old, new);
        for (text, ranges) in [(old, old_ranges), (new, new_ranges)] {
            let start = text.find("bracket the wait itself").unwrap();
            let end = start + "bracket the wait itself".len();
            assert!(!ranges.is_empty());
            assert!(ranges.iter().all(|r| r.end <= start || r.start >= end));
        }
        for added in [false, true] {
            assert!(highlighted(&hunks[0].rows, added)
                .iter()
                .any(|s| !s.is_empty()));
        }
    }

    #[test]
    fn block_diff_handles_unicode_and_one_sided_edits() {
        let patch = "diff --git a/f b/f\n@@ -1,2 +1,3 @@\n-café α stays\n-tail stays\n+café β stays\n+inserted words\n+tail stays\n";
        let diff = parse_patch(patch);
        let rows = &diff.files[0].hunks[0].rows;
        assert_eq!(highlighted(rows, false), vec!["α", ""]);
        assert_eq!(highlighted(rows, true), vec!["β", "inserted words", ""]);
        for (old, new) in [
            ("let value = 1;", "let value = 1 + 2;"),
            ("let value = 1 + 2;", "let value = 1;"),
        ] {
            let hunks = diff_texts(old, new, 0);
            assert!(
                highlighted(&hunks[0].rows, false)
                    .iter()
                    .any(|s| !s.is_empty())
                    || highlighted(&hunks[0].rows, true)
                        .iter()
                        .any(|s| !s.is_empty())
            );
        }
    }

    #[test]
    fn oversized_blocks_skip_highlights() {
        for (old, new) in [
            (
                "old value\n".repeat(MAX_BLOCK_LINES + 1),
                "new value\n".repeat(MAX_BLOCK_LINES + 1),
            ),
            (
                format!("{} old", "x".repeat(MAX_LINE_BYTES)),
                format!("{} new", "x".repeat(MAX_LINE_BYTES)),
            ),
            (
                format!("{}old", "x ".repeat(MAX_BLOCK_TOKENS)),
                format!("{}new", "x ".repeat(MAX_BLOCK_TOKENS)),
            ),
        ] {
            let hunks = diff_texts(&old, &new, 0);
            for added in [false, true] {
                assert!(highlighted(&hunks[0].rows, added)
                    .iter()
                    .all(String::is_empty));
            }
        }
    }

    #[test]
    fn consecutive_title_changes_form_one_highlight() {
        let old_title = "Add deactivation filters to the users manage list";
        let new_title = "Replace every loading spinner with a tiny migrating goose";
        let old = format!("    \"title\": \"{old_title}\",");
        let new = format!("    \"title\": \"{new_title}\",");
        let hunks = diff_texts(&old, &new, 0);
        for (row, title) in hunks[0].rows.iter().zip([old_title, new_title]) {
            let (text, intra) = match row {
                DiffRow::Removed { text, intra, .. } | DiffRow::Added { text, intra, .. } => {
                    (text, intra)
                }
                _ => panic!("expected changed row"),
            };
            let start = text.find(title).unwrap();
            assert_eq!(intra, &vec![start..start + title.len()]);
        }
    }

    #[test]
    fn whitespace_merging_preserves_shared_words_and_punctuation() {
        for (old, new) in [
            (
                "prefix old shared value suffix",
                "prefix new shared item suffix",
            ),
            ("prefix old, value suffix", "prefix new, item suffix"),
        ] {
            let (removed, added) = word_diff(old, new);
            assert_eq!(removed.len(), 2);
            assert_eq!(added.len(), 2);
        }
        let old = "prefix café\tα suffix";
        let new = "prefix tea\tβ suffix";
        let (removed, added) = word_diff(old, new);
        assert_eq!(removed, vec![7.."prefix café\tα".len()]);
        assert_eq!(added, vec![7.."prefix tea\tβ".len()]);
    }

    #[test]
    fn word_diff_highlights_changed_identifier() {
        let (old_ranges, new_ranges) = word_diff("let foo = 1;", "let bar = 1;");
        assert_eq!(old_ranges, vec![4..7]);
        assert_eq!(new_ranges, vec![4..7]);
    }

    #[test]
    fn word_diff_skips_total_rewrites() {
        let (old_ranges, new_ranges) =
            word_diff("completely different content", "nothing shared at all!");
        assert!(old_ranges.is_empty());
        assert!(new_ranges.is_empty());
    }

    #[test]
    fn intra_line_set_on_paired_run() {
        let diff = parse_patch(SAMPLE);
        match &diff.files[0].hunks[1].rows[1] {
            DiffRow::Removed { intra, .. } => assert_eq!(intra, &vec![0..3]),
            other => panic!("expected removed row, got {other:?}"),
        }
        match &diff.files[0].hunks[1].rows[2] {
            DiffRow::Added { intra, .. } => assert_eq!(intra, &vec![0..3]),
            other => panic!("expected added row, got {other:?}"),
        }
    }

    fn lines(n: Range<u32>) -> String {
        n.map(|i| format!("line {i}\n")).collect()
    }

    #[test]
    fn diff_texts_identical_produces_no_hunks() {
        let text = lines(1..20);
        assert!(diff_texts(&text, &text, 3).is_empty());
        assert!(diff_texts("", "", 3).is_empty());
    }

    #[test]
    fn diff_texts_single_change_mid_file() {
        let old = lines(1..21);
        let new = old.replace("line 10\n", "line ten\n");
        let hunks = diff_texts(&old, &new, 3);
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        assert_eq!(
            (h.old_start, h.old_count, h.new_start, h.new_count),
            (7, 7, 7, 7)
        );
        assert_eq!(h.rows.len(), 8); // 3 ctx + 1 removed + 1 added + 3 ctx
        match &h.rows[0] {
            DiffRow::Context {
                old_no,
                new_no,
                text,
            } => {
                assert_eq!((*old_no, *new_no), (7, 7));
                assert_eq!(text, "line 7");
            }
            other => panic!("expected context, got {other:?}"),
        }
        match &h.rows[3] {
            DiffRow::Removed {
                old_no,
                text,
                intra,
            } => {
                assert_eq!(*old_no, 10);
                assert_eq!(text, "line 10");
                // Intra-line pass ran on the paired change.
                assert!(!intra.is_empty());
            }
            other => panic!("expected removed, got {other:?}"),
        }
        match &h.rows[4] {
            DiffRow::Added { new_no, text, .. } => {
                assert_eq!(*new_no, 10);
                assert_eq!(text, "line ten");
            }
            other => panic!("expected added, got {other:?}"),
        }
    }

    #[test]
    fn diff_texts_merges_close_changes_and_splits_far_ones() {
        let old = lines(1..30);
        // Changes at lines 10 and 16: gap of 5 unchanged lines ≤ 2*3 → merge.
        let new = old
            .replace("line 10\n", "line X\n")
            .replace("line 16\n", "line Y\n");
        let hunks = diff_texts(&old, &new, 3);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_start, 7);
        assert_eq!(hunks[0].old_count, 13); // lines 7..=19

        // Changes at lines 5 and 20: gap of 14 > 6 → two hunks.
        let new = old
            .replace("line 5\n", "line X\n")
            .replace("line 20\n", "line Y\n");
        let hunks = diff_texts(&old, &new, 3);
        assert_eq!(hunks.len(), 2);
        assert_eq!((hunks[0].old_start, hunks[0].old_count), (2, 7));
        assert_eq!((hunks[1].old_start, hunks[1].old_count), (17, 7));
    }

    #[test]
    fn diff_texts_clamps_context_at_file_edges() {
        let old = lines(1..10);
        let new = old.replace("line 1\n", "line one\n");
        let hunks = diff_texts(&old, &new, 3);
        assert_eq!(hunks.len(), 1);
        assert_eq!((hunks[0].old_start, hunks[0].old_count), (1, 4)); // 1 change + 3 trailing ctx

        let new = old.replace("line 9\n", "line nine\n");
        let hunks = diff_texts(&old, &new, 3);
        assert_eq!(hunks.len(), 1);
        assert_eq!((hunks[0].old_start, hunks[0].old_count), (6, 4)); // 3 leading ctx + 1 change
    }

    #[test]
    fn diff_texts_crlf_produces_no_phantom_hunks() {
        let old = "alpha\r\nbeta\r\ngamma\r\n";
        let new = "alpha\nbeta\ngamma\n";
        assert!(diff_texts(old, new, 3).is_empty());
        // Mixed endings plus a real change: exactly the real change, no \r in texts.
        let new = "alpha\nbeta!\ngamma\n";
        let hunks = diff_texts(old, new, 3);
        assert_eq!(hunks.len(), 1);
        for row in &hunks[0].rows {
            let text = match row {
                DiffRow::Context { text, .. }
                | DiffRow::Added { text, .. }
                | DiffRow::Removed { text, .. } => text,
            };
            assert!(!text.contains('\r'), "phantom CR in {text:?}");
        }
    }

    #[test]
    fn diff_texts_trailing_newline_semantics() {
        // Documented choice: trailing-newline-only differences are invisible.
        assert!(diff_texts("a\nb\n", "a\nb", 3).is_empty());
        // A real change in a file without a trailing newline doesn't panic and
        // still includes the last line.
        let hunks = diff_texts("a\nb\nc", "a\nB\nc", 3);
        assert_eq!(hunks.len(), 1);
        assert_eq!((hunks[0].old_start, hunks[0].old_count), (1, 3));
        match hunks[0].rows.last().unwrap() {
            DiffRow::Context {
                old_no,
                new_no,
                text,
            } => {
                assert_eq!((*old_no, *new_no), (3, 3));
                assert_eq!(text, "c");
            }
            other => panic!("expected context, got {other:?}"),
        }
    }

    #[test]
    fn diff_texts_added_and_deleted_files() {
        // Old side empty (added file): zero-count side records line 0.
        let hunks = diff_texts("", "a\nb\n", 3);
        assert_eq!(hunks.len(), 1);
        assert_eq!(
            (
                hunks[0].old_start,
                hunks[0].old_count,
                hunks[0].new_start,
                hunks[0].new_count
            ),
            (0, 0, 1, 2)
        );
        assert!(hunks[0]
            .rows
            .iter()
            .all(|r| matches!(r, DiffRow::Added { .. })));
        // Deleted file mirrors it.
        let hunks = diff_texts("a\nb\n", "", 3);
        assert_eq!(
            (
                hunks[0].old_start,
                hunks[0].old_count,
                hunks[0].new_start,
                hunks[0].new_count
            ),
            (1, 2, 0, 0)
        );
    }

    #[test]
    fn hunk_header_without_counts() {
        let hunk = parse_hunk_header("@@ -5 +7 @@").unwrap();
        assert_eq!(
            (
                hunk.old_start,
                hunk.old_count,
                hunk.new_start,
                hunk.new_count
            ),
            (5, 1, 7, 1)
        );
    }
}
