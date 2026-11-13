//! Persistent app sessions, independent of updates and the confirmed GitHub
//! cache. Unsaved editors are restored, never auto-submitted.
use crate::*;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub(crate) const KEY: &str = "session_v1";
const LEGACY_KEY: &str = "update_restart_v1";
pub(crate) const CHECKPOINT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// A fresh launch must leave the previous session untouched until it quits.
/// Normal launches checkpoint changed state for crash recovery.
pub(crate) struct Checkpoint {
    enabled: bool,
    last_saved: Option<serde_json::Value>,
    finished: bool,
}

impl Checkpoint {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            last_saved: None,
            finished: false,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
enum SavedSource {
    Pr(github::PrLocator),
    Local {
        repo_root: PathBuf,
        branch: String,
        base_ref: Option<String>,
        base_label: String,
        base_oid: Option<String>,
    },
}

impl From<&Source> for SavedSource {
    fn from(source: &Source) -> Self {
        match source {
            Source::Pr(loc) => Self::Pr(loc.clone()),
            Source::Local(s) => Self::Local {
                repo_root: s.repo_root.clone(),
                branch: s.branch.clone(),
                base_ref: s.base_ref.clone(),
                base_label: s.base_label.clone(),
                base_oid: s.base_oid.clone(),
            },
        }
    }
}

impl From<SavedSource> for Source {
    fn from(source: SavedSource) -> Self {
        match source {
            SavedSource::Pr(loc) => Self::Pr(loc),
            SavedSource::Local {
                repo_root,
                branch,
                base_ref,
                base_label,
                base_oid,
            } => Self::Local(git::LocalSource {
                repo_root,
                branch,
                base_ref,
                base_label,
                base_oid,
            }),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct SavedView {
    position: Option<local_store::ReviewPosition>,
    local_review: Option<LocalReview>,
    comments_visible: bool,
    conversation_offset: (f32, f32),
    // Preserve the actual incremental base, not "last review" re-evaluated later.
    pub(crate) review_base: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct SavedItem {
    source: SavedSource,
    pane: ItemPane,
    view: Option<SavedView>,
    summary: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct SavedComposer {
    item: usize,
    placement: ComposerPlacement,
    reply_to: Option<u64>,
    commit_id: String,
    path: String,
    side: CommentSide,
    line: u64,
    start_line: Option<u64>,
    position: Option<github::CommentPosition>,
    pending: bool,
    edit_id: Option<u64>,
    edit_target: Option<CommentTarget>,
    row_ix: usize,
    body: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SavedSession {
    items: Vec<SavedItem>,
    active: usize,
    show_queue: bool,
    minimap: bool,
    zoom: f32,
    queue_selected: usize,
    queue_scroll: (usize, f32),
    composers: Vec<SavedComposer>,
}

pub(crate) fn read(store: &local_store::LocalStore) -> Option<SavedSession> {
    // Preserve sessions saved by the original update-only implementation. The
    // next successful save writes the new key, which takes precedence here.
    let value = store.setting(KEY).or_else(|| store.setting(LEGACY_KEY))?;
    if value.is_null() {
        return None;
    }
    match serde_json::from_value(value) {
        Ok(session) => Some(session),
        Err(error) => {
            eprintln!("gander: couldn't restore session: {error}");
            None
        }
    }
}

impl ReviewApp {
    fn session_snapshot(&self, cx: &App) -> anyhow::Result<serde_json::Value> {
        let session = SavedSession {
            active: self.active,
            show_queue: self.show_queue,
            minimap: self.minimap_visible,
            zoom: text_size(),
            queue_selected: self.queue.selected,
            queue_scroll: {
                let offset = self.queue.scroll.logical_scroll_top();
                (offset.item_ix, f32::from(offset.offset_in_item))
            },
            items: self
                .items
                .iter()
                .map(|item| SavedItem {
                    source: (&item.source).into(),
                    pane: item.pane,
                    summary: self
                        .review
                        .as_ref()
                        .filter(|r| r.item_id == item.id)
                        .map(|r| r.input.read(cx).value().to_string())
                        .or_else(|| {
                            item.review_draft
                                .as_ref()
                                .map(|r| r.input.read(cx).value().to_string())
                        }),
                    view: match &item.state {
                        ItemState::Ready(data) if data.diff_loaded => Some(SavedView {
                            position: data
                                .search
                                .as_ref()
                                .map(|s| s.prior.clone())
                                .or_else(|| data.review_position()),
                            local_review: data.local_review.clone(),
                            comments_visible: data.comments_visible,
                            conversation_offset: (
                                f32::from(data.conv_scroll.offset().x),
                                f32::from(data.conv_scroll.offset().y),
                            ),
                            review_base: data.review_base.clone(),
                        }),
                        _ => self.session_restore.get(&item.id).cloned(),
                    },
                })
                .collect(),
            composers: self
                .composers
                .iter()
                .filter_map(|c| {
                    Some(SavedComposer {
                        item: self.items.iter().position(|i| i.id == c.item_id)?,
                        placement: c.placement.clone(),
                        reply_to: c.reply_to,
                        commit_id: c.commit_id.clone(),
                        path: c.path.clone(),
                        side: c.side,
                        line: c.line,
                        start_line: c.start_line,
                        position: c.position.clone(),
                        pending: c.pending,
                        edit_id: c.edit_id,
                        edit_target: c.edit_target.clone(),
                        row_ix: c.row_ix,
                        body: c.input.read(cx).value().to_string(),
                    })
                })
                .collect(),
        };
        Ok(serde_json::to_value(session)?)
    }

    fn write_session(&mut self, snapshot: serde_json::Value) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.store.path().is_some() || cfg!(test),
            "no persistent store available"
        );
        self.store.set_setting_checked(KEY, snapshot.clone())?;
        self.session_checkpoint.last_saved = Some(snapshot);
        Ok(())
    }

    /// Always flush on quit/close and before an explicit update. A failed write
    /// must leave the previous snapshot intact and prevent an update restart.
    pub(crate) fn save_session(&mut self, cx: &App) -> anyhow::Result<()> {
        self.write_session(self.session_snapshot(cx)?)?;
        self.persist_positions();
        window_size::persist(&self.store, self.window_size);
        Ok(())
    }

    pub(crate) fn finish_app_session(&mut self, cx: &App) {
        self.session_checkpoint.enabled = false;
        // Window close, process quit and entity release can all fire. Save at
        // the first boundary, not when the last asynchronous UI handle drops.
        if !self.session_checkpoint.finished {
            match self.save_session(cx) {
                Ok(()) => self.session_checkpoint.finished = true,
                Err(error) => eprintln!("gander: couldn't save session: {error:#}"),
            }
        }
    }

    pub(crate) fn checkpoint_session(&mut self, cx: &App) -> anyhow::Result<()> {
        if !self.session_checkpoint.enabled {
            return Ok(());
        }
        let snapshot = self.session_snapshot(cx)?;
        if self.session_checkpoint.last_saved.as_ref() != Some(&snapshot) {
            self.write_session(snapshot)?;
        }
        Ok(())
    }

    pub(crate) fn spawn_session_checkpoint(&self, cx: &mut Context<Self>) {
        if !self.session_checkpoint.enabled || self.store_error.is_some() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let mut last_error = None;
            loop {
                cx.background_executor().timer(CHECKPOINT_INTERVAL).await;
                match this.update(cx, |app, cx| {
                    app.session_checkpoint
                        .enabled
                        .then(|| app.checkpoint_session(cx))
                }) {
                    Err(_) => break,
                    Ok(None) => break,
                    Ok(Some(Ok(()))) => last_error = None,
                    Ok(Some(Err(error))) => {
                        let error = format!("{error:#}");
                        if last_error.as_ref() != Some(&error) {
                            eprintln!("gander: couldn't checkpoint session: {error}");
                            last_error = Some(error);
                        }
                    }
                }
            }
        })
        .detach();
    }

    pub(crate) fn restore_session(
        &mut self,
        session: SavedSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if session.zoom.is_finite() {
            FONT_PX.store(
                session.zoom.clamp(MIN_TEXT_SIZE, MAX_TEXT_SIZE) as u32,
                Ordering::Relaxed,
            );
        }
        self.queue.selected = session
            .queue_selected
            .min(self.queue.prs.len().saturating_sub(1));
        self.queue.scroll.scroll_to(ListOffset {
            item_ix: session
                .queue_scroll
                .0
                .min(self.queue.prs.len().saturating_sub(1)),
            offset_in_item: px(session.queue_scroll.1),
        });
        for saved in session.items {
            // open_item starts the asynchronous fetch, which must capture the
            // saved scope before any work is dispatched.
            if let Some(view) = saved.view {
                self.session_restore.insert(self.next_id, view);
            }
            self.open_item(saved.source.into(), cx);
            let item = &mut self.items[self.active];
            item.pane = saved.pane;
            if let Some(summary) = saved.summary {
                let input =
                    cx.new(|cx| comment_editor::new(window, cx).placeholder("review summary…"));
                input.update(cx, |state, cx| {
                    comment_editor::set_value_at_end(state, summary, window, cx)
                });
                let uploads = match &item.source {
                    Source::Pr(loc) => Some(comment_media::Uploads::new(loc.clone(), &input, cx)),
                    _ => None,
                };
                item.review_draft = Some(ReviewDraft { input, uploads });
            }
        }
        self.active = session.active.min(self.items.len().saturating_sub(1));
        self.show_queue = session.show_queue || self.items.is_empty();
        self.minimap_visible = session.minimap;
        for saved in session.composers {
            let Some(item) = self.items.get(saved.item) else {
                continue;
            };
            let input =
                cx.new(|cx| comment_editor::new(window, cx).placeholder("leave a comment…"));
            input.update(cx, |state, cx| {
                comment_editor::set_value_at_end(state, saved.body, window, cx)
            });
            let uploads = match &item.source {
                Source::Pr(loc) => Some(comment_media::Uploads::new(loc.clone(), &input, cx)),
                _ => None,
            };
            self.composer_gen += 1;
            self.composers.push(Composer {
                id: self.composer_gen,
                item_id: item.id,
                placement: saved.placement,
                reply_to: saved.reply_to,
                commit_id: saved.commit_id,
                path: saved.path,
                side: saved.side,
                line: saved.line,
                start_line: saved.start_line,
                position: saved.position,
                pending: saved.pending,
                edit_id: saved.edit_id,
                edit_target: saved.edit_target,
                row_ix: saved.row_ix,
                input,
                uploads,
                preview: false,
                height: Rc::new(Cell::new(
                    comment_editor::minimum_height(cx) + window.rem_size() * 5.5 + px(8.),
                )),
                reveal: Rc::new(Cell::new(false)),
                error: None,
                in_flight: false,
            });
        }
        // Do not consume the snapshot. If startup fails or the process crashes
        // before the next checkpoint, the last saved session is still usable.
    }

    pub(crate) fn restore_session_view(&mut self, id: u64) {
        let Some(item) = self.items.iter_mut().find(|item| item.id == id) else {
            return;
        };
        let ItemState::Ready(data) = &mut item.state else {
            return;
        };
        if !data.diff_loaded {
            return;
        }
        let Some(view) = self.session_restore.remove(&id) else {
            return;
        };
        data.comments_visible = view.comments_visible;
        data.conv_scroll.set_offset(point(
            px(view.conversation_offset.0),
            px(view.conversation_offset.1),
        ));
        if let Some(local) = view.local_review {
            data.comments = Some(local.index());
            data.local_review = Some(local);
        }
        for c in self
            .composers
            .iter()
            .filter(|c| c.item_id == id && c.placement == ComposerPlacement::Diff)
        {
            if let Some(edit) = c.edit_id {
                data.editing_comments.insert(edit, c.height.clone());
            } else if let Some(reply) = c.reply_to {
                data.replying_comments.insert(reply, c.height.clone());
            } else {
                data.new_comments.insert(
                    c.path.clone(),
                    NewCommentEditor {
                        side: c.side,
                        line: c.line,
                        height: c.height.clone(),
                    },
                );
            }
        }
        data.rebuild_rows_anchored();
        if let Some(mut position) = view.position {
            if view.review_base != data.review_base {
                // A comparison can become unavailable after a force push. New
                // side lines remain meaningful; old-side-only anchors don't.
                position.old_line = None;
                position.row_delta = 0;
            }
            data.restore_review_position(position, true);
        }
    }
}
