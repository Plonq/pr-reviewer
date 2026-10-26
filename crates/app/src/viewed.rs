//! Per-viewer GitHub file state. Never persisted as local review bookkeeping.

use std::collections::HashSet;

use github::{ClientError, FileViewedState, ViewedFiles};
use gpui::{AppContext as _, Context, SharedString};

use crate::{ItemState, ReviewApp, Source};

#[derive(Default)]
pub(crate) struct Viewed {
    pub(crate) fetched: Option<ViewedFiles>,
    pub(crate) pending: HashSet<String>,
    pub(crate) uncertain: HashSet<String>,
    pub(crate) error: Option<SharedString>,
}

impl Viewed {
    pub(crate) fn install(&mut self, result: Option<Result<ViewedFiles, ClientError>>) {
        *self = Self::default();
        match result {
            Some(Ok(files)) => self.fetched = Some(files),
            Some(Err(error)) => {
                self.error =
                    Some(format!("Viewed state unavailable: {error}. Press r to retry.").into());
            }
            None => {}
        }
    }

    pub(crate) fn state(&self, path: &str) -> Option<FileViewedState> {
        self.fetched.as_ref()?.files.get(path).copied()
    }
}

impl ReviewApp {
    pub(crate) fn toggle_current_viewed(&mut self, cx: &mut Context<Self>) {
        if let Some(data) = self.active_data() {
            self.toggle_file_viewed(data.file_ix, cx);
        }
    }

    pub(crate) fn toggle_file_viewed(&mut self, file_ix: usize, cx: &mut Context<Self>) {
        if self.show_queue {
            return;
        }
        let Some(item) = self.items.get_mut(self.active) else {
            return;
        };
        // A refresh and a mutation must not race: refresh snapshots could
        // otherwise overwrite a newer optimistic (or confirmed) toggle.
        if item.reloading {
            return;
        }
        let Source::Pr(loc) = &item.source else {
            return;
        };
        let ItemState::Ready(data) = &mut item.state else {
            return;
        };
        let Some(file) = data.diff.files.get(file_ix) else {
            return;
        };
        let path = file.display_path().to_owned();
        if data.viewed.uncertain.contains(&path) {
            return;
        }
        let Some(previous) = data.viewed.state(&path) else {
            return;
        };
        // Serialize per path. Other files may sync independently; repeated
        // clicks while this checkbox is disabled cannot reorder mutations.
        if !data.viewed.pending.insert(path.clone()) {
            return;
        }
        let viewed = previous != FileViewedState::Viewed;
        let files = data.viewed.fetched.as_mut().unwrap();
        let pr_id = files.pull_request_id.clone();
        files.files.insert(
            path.clone(),
            if viewed {
                FileViewedState::Viewed
            } else {
                FileViewedState::Unviewed
            },
        );
        let (id, loc) = (item.id, loc.clone());
        let github = self.github.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let request_path = path.clone();
            let (result, reconciled) = cx.background_spawn(async move {
                let result = github.set_file_viewed(&pr_id, &request_path, viewed);
                // A failed transport may still have committed the mutation.
                // Ask GitHub rather than assuming a rollback is authoritative.
                let reconciled = result.as_ref().err().map(|_| github.viewed_files(&loc));
                (result, reconciled)
            }).await;
            this.update(cx, |app, cx| {
                let Some(item) = app.items.iter_mut().find(|item| item.id == id) else { return };
                let ItemState::Ready(data) = &mut item.state else { return };
                data.viewed.pending.remove(&path);
                if let Err(error) = result {
                    let state = reconciled.and_then(Result::ok).and_then(|files| files.files.get(&path).copied());
                    let files = &mut data.viewed.fetched.as_mut().unwrap().files;
                    if let Some(state) = state {
                        files.insert(path.clone(), state);
                    } else {
                        // Restore the last known value, but disable this file
                        // until a refresh confirms it; its state is uncertain.
                        files.insert(path.clone(), previous);
                        data.viewed.uncertain.insert(path.clone());
                    }
                    data.viewed.error = Some(format!(
                        "Could not sync viewed state for {path}: {error}. {} Press r to refresh.",
                        if state.is_some() { "Reloaded GitHub's state." } else { "State could not be verified; this checkbox is disabled." }
                    ).into());
                }
                cx.notify();
            }).ok();
        }).detach();
    }
}
