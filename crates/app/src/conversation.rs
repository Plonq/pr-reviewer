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
//! tradeoff holds. Jira keys are then linked in parsed Markdown prose only,
//! leaving code and existing links untouched.

mod jira;
mod markdown;
pub(crate) mod posting;
pub(crate) use markdown::render as render_markdown;

/// Expand GitHub-isms and Jira references in a description or comment body
/// into plain GFM:
///
/// - `:shortcode:` → the emoji character, via the gemoji table,
/// - `@login` → a link to the GitHub profile,
/// - `#123` → a link to the issue in `repo_slug` (GitHub redirects pulls
///   and issues to the same place; `issues/` is the canonical spelling),
/// - `PYAPI-123`, `BOT-123`, `CS-123`, `HELP-123` → Jira ticket links.
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
    jira::link_references(out)
}

/// Submitted reviews occupy one slot, with their summary before their inline
/// threads. Unassociated threads (including old cached data) stand alone.
pub(crate) enum Entry<'a> {
    Comment(&'a github::DiscussionComment),
    Review(&'a github::DiscussionComment, Vec<&'a github::ReviewThread>),
    Thread(&'a github::ReviewThread),
}

impl Entry<'_> {
    fn key(&self) -> (&str, u8, u64, &str) {
        match self {
            Self::Comment(c) => (&c.created_at, u8::from(c.review_state.is_some()), c.id, ""),
            Self::Review(c, _) => (&c.created_at, 1, c.id, ""),
            Self::Thread(t) => (&t.comments[0].created_at, 2, 0, &t.id),
        }
    }
}

pub(crate) fn timeline(discussion: &github::PrDiscussion) -> Vec<Entry<'_>> {
    let mut reviews = std::collections::HashMap::new();
    let mut entries: Vec<_> = discussion
        .comments
        .iter()
        .enumerate()
        .map(|(ix, comment)| {
            if comment.review_state.is_some() {
                if let Some(id) = &comment.review_id {
                    reviews.insert(id.as_str(), ix);
                }
                Entry::Review(comment, Vec::new())
            } else {
                Entry::Comment(comment)
            }
        })
        .collect();
    let mut threads: Vec<_> = discussion
        .threads
        .iter()
        .filter(|t| !t.comments.is_empty())
        .collect();
    threads.sort_by(|a, b| Entry::Thread(a).key().cmp(&Entry::Thread(b).key()));
    for thread in threads {
        let review = thread.comments[0]
            .review_id
            .as_deref()
            .and_then(|id| reviews.get(id));
        if let Some(Entry::Review(_, threads)) = review.and_then(|&ix| entries.get_mut(ix)) {
            threads.push(thread);
        } else {
            entries.push(Entry::Thread(thread));
        }
    }
    entries.sort_by(|a, b| a.key().cmp(&b.key()));
    // GitHub emits a bodyless COMMENTED review for inline comments, and can
    // retain it after its threads disappear. It is not an empty comment card.
    entries.retain(|entry| match entry {
        Entry::Review(comment, threads) => {
            !threads.is_empty()
                || !matches!(comment.review_state.as_deref(), Some("COMMENTED"))
                || markdown::has_content(&comment.body)
                || comment.reactions.iter().any(|reaction| reaction.count > 0)
        }
        _ => true,
    });
    entries
}

#[derive(Debug, PartialEq, Eq)]
struct CodeLine<'a> {
    old: Option<u64>,
    new: Option<u64>,
    kind: char,
    text: &'a str,
    selected: bool,
}

/// GitHub's captured hunk uses original coordinates, even if the current
/// anchor has moved. Keep a little context and end at the commented line.
fn code_excerpt(thread: &github::ReviewThread) -> Vec<CodeLine<'_>> {
    if thread.diff_hunk.is_empty() {
        return thread
            .code_context
            .as_ref()
            .map(|context| {
                context
                    .lines
                    .iter()
                    .enumerate()
                    .map(|(ix, text)| {
                        let number = context.first_line + ix as u64;
                        CodeLine {
                            old: (thread.side == "LEFT").then_some(number),
                            new: (thread.side != "LEFT").then_some(number),
                            kind: ' ',
                            text,
                            selected: number >= context.start_line.unwrap_or(context.line)
                                && number <= context.line,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
    }
    let end = thread.original_line.or(thread.line);
    let start = if thread.original_line.is_some() {
        thread.original_start_line
    } else {
        thread.start_line
    }
    .or(end);
    let mut lines = Vec::new();
    let (mut old, mut new) = (0, 0);
    for line in thread.diff_hunk.lines() {
        if line.starts_with("@@ ") {
            let mut fields = line.split_whitespace().skip(1);
            let number = |s: &str| s.get(1..)?.split(',').next()?.parse::<u64>().ok();
            let position = fields
                .next()
                .and_then(number)
                .zip(fields.next().and_then(number));
            (old, new) = position.unwrap_or((0, 0));
            continue;
        }
        if old == 0 && new == 0 {
            continue;
        }
        let Some(kind @ (' ' | '+' | '-')) = line.chars().next() else {
            continue;
        };
        let old_line = (kind != '+' && old > 0).then_some(old);
        let new_line = (kind != '-' && new > 0).then_some(new);
        let number = if thread.side == "LEFT" {
            old_line
        } else {
            new_line
        };
        lines.push(CodeLine {
            old: old_line,
            new: new_line,
            kind,
            text: &line[1..],
            selected: number
                .zip(start.zip(end))
                .is_some_and(|(n, (start, end))| n >= start && n <= end),
        });
        if kind != '+' {
            old += 1;
        }
        if kind != '-' {
            new += 1;
        }
    }
    if let Some(last) = lines.iter().rposition(|line| line.selected) {
        let first = lines.iter().position(|line| line.selected).unwrap_or(last);
        let from = first.saturating_sub(3).max((last + 1).saturating_sub(20));
        lines.truncate(last + 1);
        lines.drain(..from);
    } else if lines.len() > 6 {
        // File-level/legacy anchors may have no line; still show captured code.
        lines.drain(..lines.len() - 6);
    }
    lines
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

use crate::{
    text_size, theme, CommentSide, ComposerPlacement, ItemData, ItemState, ReviewApp, ReviewItem,
    Source,
};
use gpui::component::{
    button::{Button, ButtonVariants},
    link::Link,
    tooltip::Tooltip,
    Disableable, Sizable,
};
use gpui::{
    div, prelude::FluentBuilder, px, Context, InteractiveElement, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, Window,
};

impl ReviewApp {
    pub(crate) fn conversation_composer(
        &self,
        item_id: u64,
        thread: Option<&github::ReviewThread>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let placement = thread.map_or(ComposerPlacement::ConversationMain, |thread| {
            ComposerPlacement::ConversationThread(thread.id.clone())
        });
        if let Some(composer) = self
            .composer
            .as_ref()
            .filter(|composer| composer.item_id == item_id && composer.placement == placement)
        {
            let scroll = self
                .active_data()
                .expect("a ready conversation")
                .conv_scroll
                .clone();
            let reveal = composer.reveal.clone();
            return div()
                .w_full()
                .min_w_0()
                .on_children_prepainted(move |bounds, window, cx| {
                    let Some(card) = bounds.first() else {
                        return;
                    };
                    if !reveal.replace(false) {
                        return;
                    }
                    let viewport = scroll.bounds();
                    let delta =
                        if card.top() < viewport.top() || card.size.height > viewport.size.height {
                            viewport.top() - card.top()
                        } else if card.bottom() > viewport.bottom() {
                            viewport.bottom() - card.bottom()
                        } else {
                            return;
                        };
                    let scroll = scroll.clone();
                    let mut offset = scroll.offset();
                    offset.y += delta;
                    // Use the new card's full height, not the old button's
                    // bounds, and repaint with the adjusted scroll offset.
                    window.defer(cx, move |window, _| {
                        scroll.set_offset(offset);
                        window.refresh();
                    });
                })
                .child(self.render_composer_card(cx))
                .into_any_element();
        }
        let reply_to = thread
            .and_then(|thread| thread.comments.first())
            .map(|c| c.id);
        let path = thread.map_or(String::new(), |thread| thread.path.clone());
        let side = if thread.is_some_and(|thread| thread.side == "LEFT") {
            CommentSide::Left
        } else {
            CommentSide::Right
        };
        // A reply addresses the root identity, even for resolved/outdated
        // threads whose file or line no longer exists in the current diff.
        let line = thread
            .and_then(|thread| thread.line.or(thread.original_line))
            .unwrap_or(0);
        div()
            .debug_selector(move || {
                format!("conversation-compose-{item_id}-{}", reply_to.unwrap_or(0))
            })
            .child(
                Button::new(SharedString::from(format!(
                    "conversation-compose-{item_id}-{}",
                    reply_to.unwrap_or(0)
                )))
                .label(if thread.is_some() {
                    "Reply"
                } else {
                    "Leave a comment"
                })
                .small()
                .disabled(self.composer.is_some() || self.review.is_some())
                .on_click(cx.listener(move |this, _, window, cx| {
                    if this.active_item().map(|item| item.id) != Some(item_id) {
                        return;
                    }
                    this.open_comment_composer(
                        placement.clone(),
                        reply_to,
                        path.clone(),
                        side,
                        line,
                        0,
                        window,
                        cx,
                    );
                })),
            )
            .into_any_element()
    }

    pub(crate) fn render_commits(
        &self,
        item: &ReviewItem,
        data: &ItemData,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let item_id = item.id;
        let expanded = data.commits_expanded;
        let mut section = div()
            .mt_6()
            .min_w_0()
            .border_1()
            .border_color(theme::surface0(cx))
            .rounded_md()
            .p_4()
            .child(
                div()
                    .debug_selector(|| format!("commits-toggle-{item_id}"))
                    .child(
                        Button::new(SharedString::from(format!("commits-toggle-{item_id}")))
                            .ghost()
                            .small()
                            .label(match &data.commits {
                                Some(commits) => format!(
                                    "{} Commits ({}) · {}",
                                    if expanded { "▾" } else { "▸" },
                                    commits.commits.len(),
                                    if expanded { "collapse" } else { "expand" },
                                ),
                                None => "Commits".into(),
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let Some(item) =
                                    this.items.iter_mut().find(|item| item.id == item_id)
                                else {
                                    return;
                                };
                                if let ItemState::Ready(data) = &mut item.state {
                                    data.commits_expanded = !data.commits_expanded;
                                    cx.notify();
                                }
                            })),
                    ),
            );
        let Some(commits) = &data.commits else {
            return section
                .child(
                    div()
                        .mt_2()
                        .text_color(theme::overlay0(cx))
                        .child("Commits unavailable. Refresh to try again."),
                )
                .into_any_element();
        };
        if commits.commits.is_empty() {
            section = section.child(
                div()
                    .mt_2()
                    .text_color(theme::overlay0(cx))
                    .child("No commits."),
            );
        }
        for (ix, commit) in commits.commits.iter().enumerate() {
            if !expanded && ix + 1 != commits.commits.len() {
                continue;
            }
            let (verdict, color) = commit_ci(commit.ci, cx);
            let mut row = div()
                .debug_selector(|| format!("commit-{item_id}-{ix}"))
                .mt_3()
                .min_w_0()
                .border_t_1()
                .border_color(theme::surface0(cx))
                .pt_3();
            if expanded {
                row = row
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .child(
                                div()
                                    .text_color(theme::overlay0(cx))
                                    .child(format!("{}.", ix + 1)),
                            )
                            .child(
                                div()
                                    .font_family(crate::MONO)
                                    .text_color(theme::subtext(cx))
                                    .child(commit.sha.chars().take(7).collect::<String>()),
                            )
                            .child(
                                div()
                                    .text_color(theme::subtext(cx))
                                    .child(format!("by {}", commit.author)),
                            )
                            .child(div().text_color(color).child(verdict)),
                    )
                    // Plain text, not markdown: commit messages cannot inject links.
                    .child(div().mt_1().child(commit.message.clone()));
            } else {
                row = row.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .child(div().text_color(theme::subtext(cx)).child("Latest commit"))
                        .child(div().text_color(color).child(verdict)),
                );
            }
            for (run_ix, run) in commit.actions.iter().enumerate() {
                row = row.child(
                    div()
                        .debug_selector(|| format!("commit-action-{}-{ix}-{run_ix}", item.id))
                        .mt_1()
                        .child(
                            Link::new(SharedString::from(format!(
                                "commit-action-{}-{ix}-{run_ix}",
                                item.id
                            )))
                            .href(run.url.clone())
                            .child(format!("Actions: {} ↗", run.name)),
                        ),
                );
            }
            section = section.child(row);
        }
        section.into_any_element()
    }

    pub(crate) fn render_discussion(
        &self,
        item: &ReviewItem,
        data: &ItemData,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let Source::Pr(loc) = &item.source else {
            return Vec::new();
        };
        let empty = github::PrDiscussion::default();
        let discussion = data.discussion.as_ref().unwrap_or(&empty);
        let slug = loc.repo_slug();
        let mut elements = Vec::new();
        let entries = timeline(discussion);
        if entries.is_empty()
            && !data
                .posts
                .pending
                .iter()
                .any(|post| post.reply_to.is_none())
        {
            elements.push(
                div()
                    .mt_6()
                    .text_color(theme::overlay0(cx))
                    .child("No comments yet.")
                    .into_any_element(),
            );
        }
        for entry in entries {
            match entry {
                Entry::Comment(comment) => elements.push(
                    div()
                        .mt_6()
                        .w_full()
                        .min_w_0()
                        .border_1()
                        .border_color(theme::surface0(cx))
                        .rounded_md()
                        .p_4()
                        .child(self.discussion_comment(item.id, "main", comment, &slug, window, cx))
                        .into_any_element(),
                ),
                Entry::Review(comment, threads) => {
                    let mut review = div()
                        .mt_6()
                        .w_full()
                        .min_w_0()
                        .border_1()
                        .border_color(theme::surface0(cx))
                        .rounded_md()
                        .child(div().p_4().child(
                            self.discussion_comment(item.id, "main", comment, &slug, window, cx),
                        ));
                    if !threads.is_empty() {
                        review = review.child(
                            div()
                                .w_full()
                                .min_w_0()
                                .px_4()
                                .pb_4()
                                .flex()
                                .flex_col()
                                .gap_3()
                                .children(threads.into_iter().map(|thread| {
                                    self.discussion_thread(item, data, thread, &slug, window, cx)
                                })),
                        );
                    }
                    elements.push(review.into_any_element());
                }
                Entry::Thread(thread) => elements.push(
                    div()
                        .mt_6()
                        .child(self.discussion_thread(item, data, thread, &slug, window, cx))
                        .into_any_element(),
                ),
            }
        }
        for post in data
            .posts
            .pending
            .iter()
            .filter(|post| post.reply_to.is_none())
        {
            elements.push(
                div()
                    .mt_6()
                    .w_full()
                    .min_w_0()
                    .border_1()
                    .border_color(theme::surface0(cx))
                    .rounded_md()
                    .p_4()
                    .child(self.discussion_comment(
                        item.id,
                        "optimistic",
                        &post.comment,
                        &slug,
                        window,
                        cx,
                    ))
                    .into_any_element(),
            );
        }
        elements
    }

    fn discussion_thread(
        &self,
        item: &ReviewItem,
        data: &ItemData,
        thread: &github::ReviewThread,
        slug: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let composing = self.composer.as_ref().is_some_and(|composer| {
            composer.item_id == item.id
                && (composer.placement == ComposerPlacement::ConversationThread(thread.id.clone())
                    || thread.comments.iter().any(|comment| {
                        composer.placement
                            == ComposerPlacement::ConversationComment(
                                crate::comment_actions::published_target("review", comment).key(),
                            )
                    })
                    || self
                        .sessions
                        .get(&item.id)
                        .and_then(|ui| ui.state.pending.as_ref())
                        .is_some_and(|review| {
                            review.comments.iter().any(|draft| {
                                draft.in_reply_to_id == Some(thread.comments[0].id)
                                    && composer.placement
                                        == ComposerPlacement::ConversationComment(
                                            crate::CommentTarget::Pending(draft.id).key(),
                                        )
                            })
                        }))
        });
        let expanded =
            composing || !thread.is_resolved || data.expanded_threads.contains(&thread.id);
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
                "Unresolved · "
            },
            anchor(thread),
            thread.comments[0].author,
            thread.comments.len(),
            if thread.comments.len() == 1 { "" } else { "s" },
            if thread.is_resolved && !composing {
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
            .debug_selector(|| format!("thread-header-{}-{}", item.id, thread.id))
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .px_4()
            .py_2()
            .text_size(px(text_size() - 1.))
            .text_color(theme::subtext(cx))
            .overflow_hidden();
        if thread.is_resolved && !composing {
            header = header.child(
                div().flex_1().min_w_0().child(
                    Button::new(SharedString::from(format!(
                        "thread-toggle-{}-{}",
                        item.id, thread.id
                    )))
                    .ghost()
                    .small()
                    .w_full()
                    .label(summary)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_thread_expanded(item_id, &thread_id, cx);
                    })),
                ),
            );
        } else {
            header = header.child(div().flex_1().min_w_0().truncate().child(summary));
        }
        header = header.child(self.thread_controls(item.id, thread, false, &cx.entity(), cx));
        let mut card = div()
            .w_full()
            .flex_shrink_0()
            .min_w_0()
            .border_1()
            .border_color(theme::surface0(cx))
            .rounded_md()
            .child(header);
        if expanded {
            let excerpt = code_excerpt(thread);
            if !excerpt.is_empty() {
                card = card.child(
                    div()
                        .id(SharedString::from(format!(
                            "thread-code-{}-{}",
                            item.id, thread.id
                        )))
                        .overflow_x_scroll()
                        .border_t_1()
                        .border_color(theme::surface0(cx))
                        .font_family(crate::MONO)
                        .text_size(px(text_size() - 2.))
                        .py_2()
                        .children(excerpt.into_iter().map(|line| {
                            let mut row = div().flex().px_2().gap_2();
                            row = match line.kind {
                                '+' => row.bg(theme::added_row_bg(cx)),
                                '-' => row.bg(theme::removed_row_bg(cx)),
                                _ => row,
                            };
                            if line.selected {
                                row = row.border_l_2().border_color(theme::blue(cx));
                            } else {
                                row = row.border_l_2().border_color(gpui::transparent_black());
                            }
                            row.child(div().flex_shrink_0().text_color(theme::overlay0(cx)).child(
                                format!(
                                    "{:>4} {:>4} {}",
                                    line.old.map(|n| n.to_string()).unwrap_or_default(),
                                    line.new.map(|n| n.to_string()).unwrap_or_default(),
                                    line.kind,
                                ),
                            ))
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .whitespace_nowrap()
                                    .child(line.text.to_owned()),
                            )
                        })),
                );
            } else {
                card = card.child(
                    div()
                        .px_4()
                        .py_2()
                        .text_color(theme::overlay0(cx))
                        .child("Code context unavailable for this thread."),
                );
            }
            for (ix, comment) in thread.comments.iter().enumerate() {
                card = card.child(
                    div()
                        .p_4()
                        .when(ix > 0, |row| row.pl(px(16. + crate::threads::REPLY_INDENT)))
                        .border_t_1()
                        .border_color(theme::surface0(cx))
                        .child(
                            self.discussion_comment(item.id, "review", comment, slug, window, cx),
                        ),
                );
            }
            for post in data
                .posts
                .pending
                .iter()
                .filter(|post| post.reply_to == Some(thread.comments[0].id))
            {
                card = card.child(
                    div()
                        .p_4()
                        .pl(px(16. + crate::threads::REPLY_INDENT))
                        .border_t_1()
                        .border_color(theme::surface0(cx))
                        .child(self.discussion_comment(
                            item.id,
                            "optimistic",
                            &post.comment,
                            slug,
                            window,
                            cx,
                        )),
                );
            }
            if let Some(review) = self
                .sessions
                .get(&item.id)
                .and_then(|ui| ui.state.pending.as_ref())
            {
                let root_id = thread.comments[0].id;
                for draft in review
                    .comments
                    .iter()
                    .filter(|draft| draft.in_reply_to_id == Some(root_id))
                {
                    let target = crate::CommentTarget::Pending(draft.id);
                    let placement = ComposerPlacement::ConversationComment(target.key());
                    if self.composer.as_ref().is_some_and(|composer| {
                        composer.item_id == item.id && composer.placement == placement
                    }) {
                        card = card.child(
                            div()
                                .p_4()
                                .pl(px(16. + crate::threads::REPLY_INDENT))
                                .child(self.render_composer_card(cx)),
                        );
                        continue;
                    }
                    card = card.child(
                        div()
                            .p_4()
                            .pl(px(16. + crate::threads::REPLY_INDENT))
                            .border_t_1()
                            .border_color(theme::surface0(cx))
                            .child(
                                div()
                                    .debug_selector(|| {
                                        format!("conversation-draft-{}-{}", item.id, draft.id)
                                    })
                                    .mb_2()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .text_color(theme::yellow(cx))
                                    .child(
                                        div()
                                            .flex_1()
                                            .child("Pending reply · only you can see this"),
                                    )
                                    .child(self.comment_controls(target, placement, &cx.entity())),
                            )
                            .child(render_markdown(
                                &format!("conversation-draft-{}-{}", item.id, draft.id),
                                preprocess(&draft.body, slug),
                                window,
                                cx,
                            )),
                    );
                }
            }
            card = card.child(
                div()
                    .p_4()
                    .border_t_1()
                    .border_color(theme::surface0(cx))
                    .pl(px(16. + crate::threads::REPLY_INDENT))
                    .child(self.conversation_composer(item.id, Some(thread), cx)),
            );
        }
        card.into_any_element()
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
        let target = crate::comment_actions::published_target(kind, comment);
        let placement = ComposerPlacement::ConversationComment(target.key());
        if kind != "optimistic"
            && self.composer.as_ref().is_some_and(|composer| {
                composer.item_id == item_id && composer.placement == placement
            })
        {
            return self.render_composer_card(cx);
        }
        let timestamp = comment.created_at.clone();
        let key = self
            .items
            .iter()
            .find(|item| item.id == item_id)
            .and_then(|item| {
                if let ItemState::Ready(data) = &item.state {
                    Some(data.posts.key(kind, comment))
                } else {
                    None
                }
            })
            .unwrap_or_else(|| format!("{kind}-{}", comment.id));
        let controls = (kind != "optimistic")
            .then(|| self.comment_controls(target.clone(), placement, &cx.entity()));
        let mut card = div()
            .id(SharedString::from(format!("discussion-{item_id}-{key}")))
            .debug_selector(|| format!("discussion-{item_id}-{key}"))
            .w_full()
            .min_w_0()
            .child(
                div()
                    .mb_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_color(theme::subtext(cx))
                    .text_size(px(text_size() - 1.))
                    .child(
                        crate::threads::author_avatar(&comment.author, &self.account)
                            .debug_selector(|| format!("discussion-avatar-{item_id}-{key}")),
                    )
                    .child(
                        div()
                            .id("heading")
                            .flex_1()
                            .child(discussion_heading(comment, crate::rows::now_unix()))
                            .tooltip(move |window, cx| {
                                Tooltip::new(timestamp.clone()).build(window, cx)
                            }),
                    )
                    .children(controls),
            );
        if markdown::has_content(&comment.body) {
            card = card.child(render_markdown(
                &format!("discussion-{item_id}-{key}"),
                preprocess(&comment.body, slug),
                window,
                cx,
            ));
        }
        if kind != "optimistic"
            && (comment.reactions.iter().any(|r| r.count > 0)
                || (comment.permissions.can_react && !comment.permissions.node_id.is_empty()))
        {
            if let crate::CommentTarget::Published(target) = target {
                card = card.child(div().mt_3().child(self.comment_reactions(
                    target,
                    comment,
                    &cx.entity(),
                )));
            }
        }
        card.into_any_element()
    }
}

/// Shared by issue comments, inline roots/replies, and submitted reviews.
/// Only the presentation is relative; timeline ordering keeps the raw date.
fn discussion_heading(comment: &github::DiscussionComment, now: i64) -> String {
    let verdict = match comment.review_state.as_deref() {
        Some("APPROVED") => " · approved",
        Some("CHANGES_REQUESTED") => " · requested changes",
        Some("DISMISSED") => " · review dismissed",
        Some(_) => " · reviewed",
        None => "",
    };
    format!(
        "@{} · {}{verdict}",
        comment.author,
        crate::threads::short_age(&comment.created_at, now)
    )
}

fn commit_ci(ci: github::CiState, cx: &gpui::App) -> (&'static str, gpui::Rgba) {
    match ci {
        github::CiState::Pass => ("CI passed", theme::green(cx)),
        github::CiState::Fail => ("CI failed", theme::red(cx)),
        github::CiState::Pending => ("CI pending", theme::yellow(cx)),
        github::CiState::NoChecks => ("No checks", theme::overlay0(cx)),
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

    #[gpui::test]
    fn commits_ci_has_visible_text_and_verdict_colors(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui::init(cx);
            assert_eq!(
                commit_ci(github::CiState::Pass, cx),
                ("CI passed", theme::green(cx))
            );
            assert_eq!(
                commit_ci(github::CiState::Fail, cx),
                ("CI failed", theme::red(cx))
            );
            assert_eq!(
                commit_ci(github::CiState::Pending, cx),
                ("CI pending", theme::yellow(cx))
            );
            assert_eq!(
                commit_ci(github::CiState::NoChecks, cx),
                ("No checks", theme::overlay0(cx))
            );
        });
    }

    pub(crate) fn sample_discussion() -> github::PrDiscussion {
        let comment = |id, time: &str| github::DiscussionComment {
            permissions: Default::default(),
            id,
            author: "alice".into(),
            body: "Thanks @bob :tada: — see #12.\n\n```rust\nlet x = 1;\n```".into(),
            created_at: time.into(),
            review_state: None,
            review_id: None,
            reactions: vec![github::Reaction {
                content: "THUMBS_UP".into(),
                count: 3,
                viewer_has_reacted: false,
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
                viewer_can_resolve: true,
                viewer_can_unresolve: true,
                is_outdated: true,
                path: "src/main.rs".into(),
                side: "LEFT".into(),
                line: None,
                start_line: None,
                original_line: Some(12),
                original_start_line: Some(10),
                diff_hunk: "@@ -9,5 +9,2 @@\n fn main() {\n-    old_one();\n-    old_two();\n-    old_three();\n }".into(),
                code_context: None,
                comments: vec![
                    comment(1, "2026-01-01T12:00:00Z"),
                    comment(3, "2026-01-03T00:00:00Z"),
                ],
            }],
        }
    }

    #[test]
    fn discussion_headings_use_relative_times_and_preserve_review_verdicts() {
        let discussion = sample_discussion();
        let mut comment = discussion.comments[0].clone();
        // 2026-01-02T00:00:00Z, matching the fixture's creation time.
        let posted = 1_767_312_000;
        for (elapsed, expected) in [
            (0, "just now"),
            (60, "1m ago"),
            (3600, "1h ago"),
            (86400, "1d ago"),
            (365 * 86400, "1y ago"),
        ] {
            assert_eq!(
                discussion_heading(&comment, posted + elapsed),
                format!("@alice · {expected}")
            );
        }
        for (state, verdict) in [
            ("APPROVED", "approved"),
            ("CHANGES_REQUESTED", "requested changes"),
            ("DISMISSED", "review dismissed"),
            ("COMMENTED", "reviewed"),
        ] {
            comment.review_state = Some(state.into());
            assert_eq!(
                discussion_heading(&comment, posted + 120),
                format!("@alice · 2m ago · {verdict}")
            );
        }
        assert_eq!(comment.created_at, "2026-01-02T00:00:00Z");
        comment.review_state = None;
        comment.created_at = "unknown date".into();
        assert_eq!(
            discussion_heading(&comment, posted),
            "@alice · unknown date"
        );
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
                    Entry::Review(c, _) => format!("review-{}", c.id),
                    Entry::Thread(t) => t.id.clone(),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&discussion), vec!["comment-1", "comment-2", "T1"]);
        discussion.comments.reverse();
        assert_eq!(ids(&discussion), vec!["comment-1", "comment-2", "T1"]);
    }

    #[test]
    fn reviews_group_roots_not_replies_at_submission_time() {
        let mut discussion = sample_discussion();
        let mut review = discussion.comments[0].clone();
        review.id = 10;
        review.review_id = Some("R1".into());
        review.review_state = Some("CHANGES_REQUESTED".into());
        review.created_at = "2026-01-04T00:00:00Z".into();
        discussion.comments.push(review.clone());
        discussion.threads[0].comments[0].review_id = Some("R1".into());
        discussion.threads[0].comments[1].review_id = Some("R2".into());
        let mut second = discussion.threads[0].clone();
        second.id = "T2".into();
        discussion.threads.push(second);
        review.id = 11;
        review.review_id = Some("R2".into());
        discussion.comments.push(review);
        let entries = timeline(&discussion);
        assert_eq!(entries.len(), 4, "no duplicate standalone inline threads");
        assert!(matches!(&entries[0], Entry::Comment(c) if c.id == 2));
        assert!(matches!(&entries[1], Entry::Comment(c) if c.id == 1));
        assert!(
            matches!(&entries[2], Entry::Review(c, threads) if c.id == 10 && threads.iter().map(|t| t.id.as_str()).collect::<Vec<_>>() == ["T1", "T2"])
        );
        assert!(
            matches!(&entries[3], Entry::Review(c, threads) if c.id == 11 && threads.is_empty())
        );
        discussion.comments.retain(|c| c.id != 10);
        assert_eq!(
            timeline(&discussion).len(),
            5,
            "missing reviews retain standalone threads"
        );
    }

    #[test]
    fn empty_commented_reviews_are_hidden_but_verdicts_threads_and_reactions_remain() {
        let mut discussion = sample_discussion();
        let mut review = discussion.comments[0].clone();
        review.review_state = Some("COMMENTED".into());
        review.review_id = Some("R1".into());
        review.body = " \n <!-- bot metadata --> \n".into();
        review.reactions.clear();
        discussion.comments = vec![review.clone()];
        discussion.threads.clear();
        assert!(timeline(&discussion).is_empty());
        for state in ["APPROVED", "CHANGES_REQUESTED", "DISMISSED"] {
            discussion.comments[0].review_state = Some(state.into());
            assert_eq!(
                timeline(&discussion).len(),
                1,
                "keep meaningful {state} verdict"
            );
        }
        discussion.comments[0] = review.clone();
        discussion.comments[0].body = "Actual review summary".into();
        assert_eq!(timeline(&discussion).len(), 1);
        discussion.comments[0] = review.clone();
        discussion.comments[0].reactions = vec![github::Reaction {
            content: "THUMBS_UP".into(),
            count: 1,
            viewer_has_reacted: false,
        }];
        assert_eq!(timeline(&discussion).len(), 1);
        discussion.comments[0] = review;
        discussion.threads = sample_discussion().threads;
        discussion.threads[0].comments[0].review_id = Some("R1".into());
        assert!(
            matches!(&timeline(&discussion)[..], [Entry::Review(_, threads)] if threads.len() == 1)
        );
    }

    #[test]
    fn excerpts_use_historical_side_and_highlight_multiline_ranges() {
        let mut thread = sample_discussion().threads.remove(0);
        // The LEFT range is historical 10–12, not the shifted current anchor.
        thread.line = Some(100);
        thread.start_line = Some(98);
        let excerpt = code_excerpt(&thread);
        assert_eq!(excerpt.len(), 4);
        assert_eq!(excerpt[0].old, Some(9));
        assert_eq!(excerpt[0].new, Some(9));
        assert!(!excerpt[0].selected);
        assert!(excerpt[1..]
            .iter()
            .all(|line| line.selected && line.new.is_none()));
        assert_eq!(excerpt.last().unwrap().text, "    old_three();");

        thread.side = "RIGHT".into();
        thread.original_start_line = None;
        thread.start_line = None;
        thread.original_line = Some(11);
        thread.diff_hunk = "@@ -9,3 +9,4 @@\n context\n-old\n+new\n+    @literal #12 :rocket:\n tail\n\\ No newline at end of file".into();
        let excerpt = code_excerpt(&thread);
        let last = excerpt.last().unwrap();
        assert_eq!(last.new, Some(11));
        assert_eq!(last.old, None);
        assert!(last.selected);
        assert_eq!(last.text, "    @literal #12 :rocket:");
        thread.diff_hunk.clear();
        assert!(code_excerpt(&thread).is_empty());
    }

    #[test]
    fn outside_diff_excerpts_show_only_the_captured_side_and_range() {
        let mut thread = sample_discussion().threads.remove(0);
        thread.diff_hunk.clear();
        thread.code_context = Some(github::ReviewCodeContext {
            commit_id: "captured-old-base".into(),
            line: 88,
            start_line: Some(87),
            first_line: 86,
            lines: vec![
                "context".into(),
                "    first();".into(),
                "    @literal #12 :rocket:".into(),
            ],
        });
        for side in ["LEFT", "RIGHT"] {
            thread.side = side.into();
            let excerpt = code_excerpt(&thread);
            assert_eq!(excerpt.len(), 3);
            assert!(!excerpt[0].selected);
            assert!(excerpt[1..]
                .iter()
                .all(|line| line.selected && line.kind == ' '));
            assert_eq!(excerpt[2].text, "    @literal #12 :rocket:");
            assert_eq!(excerpt[2].old, (side == "LEFT").then_some(88));
            assert_eq!(excerpt[2].new, (side == "RIGHT").then_some(88));
        }
    }

    #[test]
    fn excerpts_bound_large_hunks_and_handle_zero_count_and_multiple_headers() {
        let mut thread = sample_discussion().threads.remove(0);
        thread.side = "RIGHT".into();
        thread.original_start_line = Some(1);
        thread.original_line = Some(100);
        thread.diff_hunk = format!("@@ -0,0 +1,100 @@\n{}", "+new\n".repeat(100));
        let excerpt = code_excerpt(&thread);
        assert_eq!(excerpt.len(), 20);
        assert_eq!(excerpt[0].new, Some(81));
        assert_eq!(excerpt.last().unwrap().new, Some(100));
        assert!(excerpt
            .iter()
            .all(|line| line.old.is_none() && line.selected));
        thread.original_start_line = None;
        thread.original_line = Some(30);
        thread.diff_hunk = "@@ -1 +1 @@\n a\n@@ -20 +30 @@\n b".into();
        assert_eq!(code_excerpt(&thread).last().unwrap().new, Some(30));
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

    #[test]
    fn jira_refs_share_the_description_and_comment_preprocessing_pipe() {
        assert_eq!(
            preprocess("Thanks @bob :tada: — #12 fixes PYAPI-1234.", "acme/widgets"),
            "Thanks [@bob](https://github.com/bob) 🎉 — \
             [#12](https://github.com/acme/widgets/issues/12) fixes \
             [PYAPI-1234](https://ucroo-platform.atlassian.net/browse/PYAPI-1234).\n"
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
