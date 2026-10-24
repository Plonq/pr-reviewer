//! The conversation timeline and its shared markdown text pipe.
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

/// Expand the GitHub-isms in a description or comment body into plain GFM:
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

/// A thread occupies one chronological slot at its first comment; replies
/// remain inside it even when main-thread comments arrive between replies.
pub(crate) enum Entry<'a> {
    Comment(&'a github::DiscussionComment),
    Thread(&'a github::ReviewThread),
}

impl Entry<'_> {
    fn key(&self) -> (&str, u8, u64, &str) {
        match self {
            Self::Comment(c) => (&c.created_at, u8::from(c.review_state.is_some()), c.id, ""),
            Self::Thread(t) => (&t.comments[0].created_at, 2, 0, &t.id),
        }
    }
}

pub(crate) fn timeline(discussion: &github::PrDiscussion) -> Vec<Entry<'_>> {
    let mut entries: Vec<_> = discussion
        .comments
        .iter()
        .map(Entry::Comment)
        .chain(
            discussion
                .threads
                .iter()
                .filter(|t| !t.comments.is_empty())
                .map(Entry::Thread),
        )
        .collect();
    entries.sort_by(|a, b| a.key().cmp(&b.key()));
    entries
}

pub(crate) fn anchor(thread: &github::ReviewThread) -> String {
    let (start, line) = if thread.line.is_some() {
        (thread.start_line, thread.line)
    } else {
        (thread.original_start_line, thread.original_line)
    };
    let range = match (start, line) {
        (Some(start), Some(end)) if start < end => format!(":{start}–{end}"),
        (_, Some(line)) => format!(":{line}"),
        _ => String::new(),
    };
    format!(
        "{}{range} · {}{}",
        thread.path,
        if thread.side == "LEFT" { "old" } else { "new" },
        if thread.is_outdated {
            " · outdated"
        } else {
            ""
        }
    )
}

use crate::{text_size, theme, ItemData, ItemState, ReviewApp, ReviewItem, Source};
use gpui::{
    div, px, rems, Context, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window,
};
use gpui_component::{
    text::{TextView, TextViewStyle},
    ActiveTheme,
};

impl ReviewApp {
    pub(crate) fn render_discussion(
        &self,
        item: &ReviewItem,
        data: &ItemData,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let (Some(discussion), Source::Pr(loc)) = (&data.discussion, &item.source) else {
            return Vec::new();
        };
        let slug = loc.repo_slug();
        let mut elements = Vec::new();
        if discussion.comments.is_empty() && discussion.threads.is_empty() {
            elements.push(
                div()
                    .mt_6()
                    .text_color(theme::overlay0())
                    .child("No comments yet.")
                    .into_any_element(),
            );
        }
        for entry in timeline(discussion) {
            match entry {
                Entry::Comment(comment) => elements.push(
                    div()
                        .mt_6()
                        .border_1()
                        .border_color(theme::surface0())
                        .rounded_md()
                        .p_4()
                        .child(self.discussion_comment(item.id, "main", comment, &slug, window, cx))
                        .into_any_element(),
                ),
                Entry::Thread(thread) => {
                    let expanded =
                        !thread.is_resolved || data.expanded_threads.contains(&thread.id);
                    let item_id = item.id;
                    let thread_id = thread.id.clone();
                    let summary = format!(
                        "{}{} · @{} · {} comment{}{}",
                        if thread.is_resolved {
                            if expanded {
                                "▾ Resolved · "
                            } else {
                                "▸ Resolved · "
                            }
                        } else {
                            "Review thread · "
                        },
                        anchor(thread),
                        thread.comments[0].author,
                        thread.comments.len(),
                        if thread.comments.len() == 1 { "" } else { "s" },
                        if thread.is_resolved {
                            if expanded {
                                " · collapse"
                            } else {
                                " · expand"
                            }
                        } else {
                            ""
                        }
                    );
                    let mut header = div()
                        .id(SharedString::from(format!(
                            "thread-{}-{}",
                            item.id, thread.id
                        )))
                        .px_4()
                        .py_2()
                        .text_size(px(text_size() - 1.))
                        .text_color(theme::subtext())
                        .overflow_hidden()
                        .child(div().truncate().child(summary));
                    if thread.is_resolved {
                        header =
                            header
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let Some(item) =
                                        this.items.iter_mut().find(|item| item.id == item_id)
                                    else {
                                        return;
                                    };
                                    let ItemState::Ready(data) = &mut item.state else {
                                        return;
                                    };
                                    if !data.expanded_threads.remove(&thread_id) {
                                        data.expanded_threads.insert(thread_id.clone());
                                    }
                                    cx.notify();
                                }));
                    }
                    let mut card = div()
                        .mt_6()
                        .border_1()
                        .border_color(theme::surface0())
                        .rounded_md()
                        .child(header);
                    if expanded {
                        for comment in &thread.comments {
                            card = card.child(
                                div()
                                    .p_4()
                                    .border_t_1()
                                    .border_color(theme::surface0())
                                    .child(self.discussion_comment(
                                        item.id, "review", comment, &slug, window, cx,
                                    )),
                            );
                        }
                    }
                    elements.push(card.into_any_element());
                }
            }
        }
        elements
    }

    fn discussion_comment(
        &self,
        item_id: u64,
        kind: &str,
        comment: &github::DiscussionComment,
        slug: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let verdict = match comment.review_state.as_deref() {
            Some("APPROVED") => " · approved",
            Some("CHANGES_REQUESTED") => " · requested changes",
            Some("DISMISSED") => " · review dismissed",
            Some(_) => " · reviewed",
            None => "",
        };
        let mut card = div().min_w_0().child(
            div()
                .mb_2()
                .text_color(theme::subtext())
                .text_size(px(text_size() - 1.))
                .child(format!(
                    "@{} · {}{verdict}",
                    comment.author, comment.created_at
                )),
        );
        if !comment.body.trim().is_empty() {
            card = card.child(
                TextView::markdown(
                    SharedString::from(format!(
                        "discussion-{item_id}-{kind}-{}-{}",
                        comment.review_state.is_some(),
                        comment.id
                    )),
                    SharedString::from(preprocess(&comment.body, slug)),
                    window,
                    cx,
                )
                .selectable(true)
                .style(TextViewStyle {
                    heading_base_font_size: px(text_size()),
                    paragraph_gap: rems(0.75),
                    highlight_theme: cx.theme().highlight_theme.clone(),
                    is_dark: !theme::is_light(),
                    ..Default::default()
                }),
            );
        }
        // Plain labels, deliberately not buttons: adding reactions is v0.2.
        let reactions: Vec<_> = comment
            .reactions
            .iter()
            .filter(|r| r.count > 0)
            .map(|reaction| {
                div()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(theme::surface0())
                    .child(format!("{} {}", reaction.emoji(), reaction.count))
            })
            .collect();
        if !reactions.is_empty() {
            card = card.child(div().mt_3().flex().flex_wrap().gap_2().children(reactions));
        }
        card.into_any_element()
    }
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
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sample_discussion() -> github::PrDiscussion {
        let comment = |id, time: &str| github::DiscussionComment {
            id,
            author: "alice".into(),
            body: "Thanks @bob :tada: — see #12.\n\n```rust\nlet x = 1;\n```".into(),
            created_at: time.into(),
            review_state: None,
            reactions: vec![github::Reaction {
                content: "THUMBS_UP".into(),
                count: 3,
            }],
        };
        github::PrDiscussion {
            comments: vec![
                comment(1, "2026-01-02T00:00:00Z"),
                comment(2, "2026-01-01T00:00:00Z"),
            ],
            threads: vec![github::ReviewThread {
                id: "T1".into(),
                is_resolved: true,
                is_outdated: true,
                path: "src/main.rs".into(),
                side: "LEFT".into(),
                line: None,
                start_line: None,
                original_line: Some(12),
                original_start_line: Some(10),
                comments: vec![
                    comment(1, "2026-01-01T12:00:00Z"),
                    comment(3, "2026-01-03T00:00:00Z"),
                ],
            }],
        }
    }

    #[test]
    fn timeline_merges_comments_and_threads_without_ungrouping_replies() {
        let discussion = sample_discussion();
        let entries = timeline(&discussion);
        assert_eq!(
            entries.len(),
            3,
            "replies never become extra timeline entries"
        );
        assert!(matches!(entries[0], Entry::Comment(c) if c.id == 2));
        assert!(
            matches!(entries[1], Entry::Thread(t) if t.comments.len() == 2 && t.comments[1].id == 3)
        );
        assert!(matches!(entries[2], Entry::Comment(c) if c.id == 1));
        assert_eq!(
            anchor(&discussion.threads[0]),
            "src/main.rs:10–12 · old · outdated"
        );
    }

    #[test]
    fn timeline_ties_are_deterministic_and_empty_threads_are_ignored() {
        let mut discussion = sample_discussion();
        let time = discussion.threads[0].comments[0].created_at.clone();
        for comment in &mut discussion.comments {
            comment.created_at = time.clone();
        }
        let mut empty = discussion.threads[0].clone();
        empty.comments.clear();
        discussion.threads.push(empty);
        let ids = |d: &github::PrDiscussion| {
            timeline(d)
                .iter()
                .map(|e| match e {
                    Entry::Comment(c) => format!("comment-{}", c.id),
                    Entry::Thread(t) => t.id.clone(),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&discussion), vec!["comment-1", "comment-2", "T1"]);
        discussion.comments.reverse();
        assert_eq!(ids(&discussion), vec!["comment-1", "comment-2", "T1"]);
    }

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
