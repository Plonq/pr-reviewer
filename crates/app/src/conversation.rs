//! The conversation pane's text pipe.
//!
//! A PR description is GitHub-flavored markdown plus three GitHub-isms that
//! plain GFM leaves as raw syntax: `:rocket:` emoji shortcodes, `@mentions`,
//! and `#123` issue references. GitHub's web UI expands these before
//! rendering; the Kit TextView renders GFM only, so this module expands them
//! first — the same preprocessing pipe — and hands the renderer markdown as
//! if the author had written the links and characters themselves. A
//! description that already arrived with links and emoji characters passes
//! through untouched.
//!
//! Crude by design (v1): the scanner is line-based. Fenced code blocks
//! (``` and ~~~) are passed through untouched, and inline `` `code` `` spans
//! are masked within a line, because nobody wants an issue link injected
//! into a snippet. But the pipe is otherwise prose-blind: markdown inside
//! other constructs (link targets, autolinks, indented code) is treated as
//! text, so a pathological description can still be rewritten a little too
//! eagerly. Task 12 reuses this pipe for comment bodies, where the same
//! tradeoff holds.

/// Expand the GitHub-isms in one body (the PR description now, comment
/// bodies later) into plain GFM:
///
/// - `:shortcode:` → the emoji character, via the gemoji table,
/// - `@login` → a link to the GitHub profile,
/// - `#123` → a link to the issue in `repo_slug` (GitHub redirects pulls
///   and issues to the same place; `issues/` is the canonical spelling).
///
/// Code is never rewritten: fenced blocks pass through whole, and inline
/// code spans are skipped within a line.
pub fn preprocess(body: &str, repo_slug: &str) -> String {
    let mut out = String::with_capacity(body.len());
    // The fence marker that would close the open block: backticks or tildes.
    // A block only closes on a run at least as long as the one that opened
    // it (CommonMark's rule); the v1 scanner just needs open/close at all.
    let mut fence: Option<char> = None;
    for line in body.split('\n') {
        // CR at line end (CRLF bodies) is not a character the pipe wants
        // to think about; markdown treats it as a line ending anyway.
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(marker) = fence {
            out.push_str(line);
            out.push('\n');
            if fence_closes(line, marker) {
                fence = None;
            }
            continue;
        }
        if let Some(marker) = fence_opens(line) {
            out.push_str(line);
            out.push('\n');
            fence = Some(marker);
            continue;
        }
        expand_line(line, repo_slug, &mut out);
        out.push('\n');
    }
    // The loop pushed a newline after every line, including the last —
    // split('\n') on a trailing-newline body ends with an empty line, so
    // `foo\n` round-trips; a body without one (`foo`) gains a harmless
    // trailing newline that markdown ignores.
    out
}

/// Whether a line opens a fenced code block, and with which marker. The
/// marker can be indented up to three spaces (CommonMark allows it); more
/// indentation than that is, in CommonMark, an indented code block the v1
/// pipe deliberately doesn't recognize — list items indent by four too,
/// and prose inside lists is exactly what must be expanded.
fn fence_opens(line: &str) -> Option<char> {
    let indent = line.len() - line.trim_start().len();
    if indent > 3 {
        return None;
    }
    let trimmed = line.trim_start();
    let marker = trimmed.chars().next()?;
    let run = trimmed.chars().take_while(|&c| c == marker).count();
    // An info string (```rust) is fine for backticks; CommonMark forbids
    // backticks in a tilde fence's info string, which the v1 pipe ignores.
    (matches!(marker, '`' | '~') && run >= 3).then_some(marker)
}

/// Whether a line closes the open fence: only the marker, three or more of
/// it, and nothing else (the info string of an opening line isn't checked).
fn fence_closes(line: &str, marker: char) -> bool {
    let trimmed = line.trim();
    let run = trimmed.chars().take_while(|&c| c == marker).count();
    run >= 3 && trimmed.len() == run
}

/// Expand one non-code line: the segments between inline code spans are
/// rewritten, the spans themselves copied verbatim. A backtick run with no
/// closing run of the same length is literal text, as in CommonMark.
fn expand_line(line: &str, repo_slug: &str, out: &mut String) {
    let mut rest = line;
    while let Some(open) = rest.find('`') {
        expand_text(&rest[..open], repo_slug, out);
        // The opening run's length; a closing run must match it exactly.
        let run = rest[open..].chars().take_while(|&c| c == '`').count();
        let after = &rest[open + run..];
        match closing_run(after, run) {
            Some(close) => {
                let close_at = open + run + close;
                out.push_str(&rest[open..close_at + run]);
                rest = &rest[close_at + run..];
            }
            None => {
                // Unterminated: literal backticks, and the scan continues
                // after them (they may be part of the prose).
                out.push_str(&rest[open..open + run]);
                rest = &rest[open + run..];
            }
        }
    }
    expand_text(rest, repo_slug, out);
}

/// Where the run of exactly `run` backticks that closes an inline code
/// span starts, if it exists (CommonMark: an opening run closes only on a
/// run of the same length).
fn closing_run(text: &str, run: usize) -> Option<usize> {
    let mut search = 0;
    while let Some(hit) = text[search..].find('`') {
        let at = search + hit;
        let crun = text[at..].chars().take_while(|&c| c == '`').count();
        if crun == run {
            return Some(at);
        }
        search = at + crun;
    }
    None
}

/// Expand the three GitHub-isms in one run of prose.
fn expand_text(text: &str, repo_slug: &str, out: &mut String) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b':' => {
                // `:shortcode:` — lowercase GitHub aliases. Anything between
                // colons that isn't in the table stays as written ("10:30"
                // is not an emoji, and survives).
                let code_end = text[i + 1..]
                    .chars()
                    .take_while(|&c| {
                        c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '+' | '-')
                    })
                    .map(|c| c.len_utf8())
                    .sum::<usize>();
                if code_end > 0 && bytes.get(i + 1 + code_end) == Some(&b':') {
                    let code = &text[i + 1..i + 1 + code_end];
                    if let Some(emoji) = emojis::get_by_shortcode(code) {
                        out.push_str(emoji.as_str());
                        i += 1 + code_end + 1;
                        continue;
                    }
                }
                out.push(':');
                i += 1;
            }
            b'@' => {
                // A mention: a login at a boundary — not glued to a word
                // (an email address is not a mention), and not trailing
                // into a domain (`@bob.com` is prose, not a mention).
                // Logins are alphanumerics with inner hyphens; trailing
                // hyphens belong to the prose (a login can't end in one).
                let prev_word = bytes.get(i.wrapping_sub(1)).is_some_and(|&b| {
                    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'/' | b'.' | b'-')
                });
                let login_end = text[i + 1..]
                    .chars()
                    .take_while(|&c| c.is_ascii_alphanumeric() || c == '-')
                    .map(|c| c.len_utf8())
                    .sum::<usize>();
                let login = text[i + 1..i + 1 + login_end].trim_end_matches('-');
                let after_login = &text[i + 1 + login.len()..];
                let emailish = after_login.starts_with('.')
                    && after_login[1..].starts_with(|c: char| c.is_ascii_alphanumeric());
                // A login starts alphanumeric (a leading `-` is prose).
                let starts_alnum = login
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphanumeric());
                if !prev_word && starts_alnum && !emailish {
                    out.push_str(&format!("[@{login}](https://github.com/{login})"));
                    i += 1 + login.len();
                } else {
                    out.push('@');
                    i += 1;
                }
            }
            b'#' => {
                // An issue reference: `#` followed by digits, at a boundary
                // — a heading has a space after its `#`, and `PR#123`
                // (no boundary) stays plain. `#0` doesn't exist, so the
                // first digit must be 1-9, as on GitHub. An HTML entity
                // (`&#39;`) is not a reference either.
                let prev = !bytes
                    .get(i.wrapping_sub(1))
                    .is_some_and(|&b| b.is_ascii_alphanumeric() || matches!(b, b'&' | b'#'));
                let digits_end = text[i + 1..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .count();
                if prev
                    && digits_end > 0
                    && text[i + 1..].starts_with(|c: char| ('1'..='9').contains(&c))
                {
                    let number = &text[i + 1..i + 1 + digits_end];
                    out.push_str(&format!(
                        "[#{number}](https://github.com/{repo_slug}/issues/{number})"
                    ));
                    i += 1 + digits_end;
                } else {
                    out.push('#');
                    i += 1;
                }
            }
            _ => {
                // Copy one non-ASCII-safe char at a time: the byte dispatch
                // above only fires on `:`, `@`, `#`, which are ASCII, so
                // multibyte sequences land here and pass through whole.
                let ch = text[i..].chars().next().expect("index on a char boundary");
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three expansions, together as one description would mix them.
    #[test]
    fn the_three_github_isms_expand() {
        let body = "Thanks @octocat for :tada: — closes #123.";
        assert_eq!(
            preprocess(body, "acme/widgets"),
            "Thanks [@octocat](https://github.com/octocat) for 🎉 — closes \
             [#123](https://github.com/acme/widgets/issues/123).\n"
        );
    }

    /// `+1` and `-1` — shortcodes that start with punctuation, the ones a
    /// naive "letters only" scanner misses — must expand too.
    #[test]
    fn punctuation_shortcodes_expand() {
        assert_eq!(preprocess(":+1: and :-1:", "acme/widgets"), "👍 and 👎\n");
    }

    /// Not every colon pair is a shortcode: times, ratios, and unknown names
    /// stay exactly as written.
    #[test]
    fn plain_colons_survive() {
        assert_eq!(
            preprocess("Meet at 10:30, ratio 1:2, :not_a_shortcode:", "a/b"),
            "Meet at 10:30, ratio 1:2, :not_a_shortcode:\n"
        );
    }

    /// Mentions only expand at a boundary: an email address is prose, not a
    /// mention, and the part after the `@` of a broken token isn't a login.
    #[test]
    fn mentions_need_a_boundary() {
        assert_eq!(
            preprocess("mail alice@example.com, cc @bob.", "a/b"),
            "mail alice@example.com, cc [@bob](https://github.com/bob).\n"
        );
        assert_eq!(
            preprocess("@carol- reviewed", "a/b"),
            "[@carol](https://github.com/carol)- reviewed\n"
        );
        // A login that trails into a domain is prose — an email in the
        // making, not a mention.
        assert_eq!(preprocess("ping @bob.com", "a/b"), "ping @bob.com\n");
        // A leading hyphen is prose; a login starts alphanumeric.
        assert_eq!(preprocess("@-abc", "a/b"), "@-abc\n");
    }

    /// Issue refs need a boundary and a real number: headings (`# title`),
    /// `#0`, and `PR#123` stay plain.
    #[test]
    fn issue_refs_need_a_boundary_and_a_number() {
        let body = "# Heading\nsee #12 and #3, not #0 or PR#9";
        assert_eq!(
            preprocess(body, "acme/widgets"),
            "# Heading\nsee [#12](https://github.com/acme/widgets/issues/12) and \
             [#3](https://github.com/acme/widgets/issues/3), not #0 or PR#9\n"
        );
    }

    /// Fenced code — backticks or tildes, with an info string — passes
    /// through whole: the code a description quotes must never be rewritten.
    #[test]
    fn fenced_code_is_untouched() {
        let body = "before\n```rust\nlet x = :rocket: @octocat #1;\n```\nafter :sparkles:";
        assert_eq!(
            preprocess(body, "a/b"),
            "before\n```rust\nlet x = :rocket: @octocat #1;\n```\nafter ✨\n"
        );
        let body = "~~~\n:eyes: @a\n~~~\ndone";
        assert_eq!(preprocess(body, "a/b"), "~~~\n:eyes: @a\n~~~\ndone\n");
    }

    /// Inline code spans are masked too, with CommonMark's rule that a run
    /// closes only on a run of the same length.
    #[test]
    fn inline_code_is_untouched() {
        assert_eq!(
            preprocess("try `@mention` and ``:tada:` #1`` here #2", "a/b"),
            "try `@mention` and ``:tada:` #1`` here \
             [#2](https://github.com/a/b/issues/2)\n"
        );
    }

    /// An unterminated backtick run is literal prose, not a code span, so
    /// the pipe keeps expanding after it.
    #[test]
    fn unterminated_backticks_are_prose() {
        assert_eq!(
            preprocess("a ` lone #1", "a/b"),
            "a ` lone [#1](https://github.com/a/b/issues/1)\n"
        );
    }

    /// Text that already is markdown links and emoji characters passes
    /// through unchanged — the pipe adds, it never rewrites.
    #[test]
    fn already_expanded_text_passes_through() {
        let body = "🚀 [link](https://x.com/a#frag) and **bold**";
        assert_eq!(preprocess(body, "a/b"), format!("{body}\n"));
    }

    /// Empty bodies stay empty (the caller shows its own "no description"
    /// state); CRLF bodies normalize to LF, which markdown doesn't mind.
    #[test]
    fn empty_and_crlf_bodies() {
        assert_eq!(preprocess("", "a/b"), "\n");
        assert_eq!(preprocess("a\r\nb", "a/b"), "a\nb\n");
        // A CR mid-line (not a line ending) survives as the character it is.
        assert_eq!(preprocess("a\rb", "a/b"), "a\rb\n");
    }
}
