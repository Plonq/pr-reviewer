//! Sparkle owns downloading, verification and installation. GPUI owns the only
//! update UI. Development builds do not link or run Sparkle.
use crate::{ItemState, ReviewApp};
use gpui::component::{
    button::{Button, ButtonVariants},
    Disableable, Sizable,
};
use gpui::{div, prelude::*, Context};

#[derive(Default)]
pub(crate) struct Updater {
    pub(crate) state: i32,
    pub(crate) error: Option<String>,
    save_error: Option<String>,
}

impl Updater {
    pub(crate) fn ready(&self) -> bool {
        if self.state == 1 {
            return true;
        }
        // Do not miss a staged update merely because the last UI poll ran just
        // before Sparkle finished downloading it.
        #[cfg(all(feature = "auto-updates", not(test)))]
        return unsafe { native::gander_updater_state() == 1 };
        #[cfg(any(not(feature = "auto-updates"), test))]
        false
    }
}

#[cfg(all(feature = "auto-updates", not(test)))]
mod native {
    unsafe extern "C" {
        pub fn gander_updater_start();
        pub fn gander_updater_state() -> i32;
        pub fn gander_updater_error() -> *const std::ffi::c_char;
        pub fn gander_updater_install();
        pub fn gander_updater_retry();
    }
}

impl ReviewApp {
    pub(crate) fn spawn_updater(&self, cx: &mut Context<Self>) {
        #[cfg(all(feature = "auto-updates", not(test)))]
        {
            // Called on GPUI's main thread, as are all subsequent native calls.
            unsafe { native::gander_updater_start() };
            cx.spawn(async move |this, cx| loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                if this
                    .update(cx, |app, cx| {
                        let state = unsafe { native::gander_updater_state() };
                        let error = unsafe {
                            let ptr = native::gander_updater_error();
                            (!ptr.is_null()).then(|| {
                                std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
                            })
                        };
                        if app.updater.state != state || app.updater.error != error {
                            app.updater.state = state;
                            app.updater.error = error;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            })
            .detach();
        }
        #[cfg(any(not(feature = "auto-updates"), test))]
        let _ = cx;
    }

    pub(crate) fn update_blocked(&self, cx: &gpui::App) -> bool {
        let uploading = |uploads: Option<&gpui::Entity<crate::comment_media::Uploads>>| {
            uploads.is_some_and(|uploads| uploads.read(cx).pending() > 0)
        };
        self.store_error.is_some()
            || self.sessions.values().any(|session| session.busy)
            || self
                .composers
                .iter()
                .any(|c| c.in_flight || uploading(c.uploads.as_ref()))
            || self
                .review
                .as_ref()
                .is_some_and(|r| r.in_flight || uploading(r.uploads.as_ref()))
            || self.items.iter().any(|item| {
                item.review_draft
                    .as_ref()
                    .is_some_and(|r| uploading(r.uploads.as_ref()))
                    || matches!(&item.state, ItemState::Ready(data)
                        if !data.viewed.pending.is_empty()
                        || !data.posts.pending.is_empty()
                        || !data.posts.failed.is_empty()
                        || !data.posts.changes.active.is_empty()
                        || !data.posts.changes.failed.is_empty())
            })
    }

    pub(crate) fn install_update(&mut self, cx: &mut Context<Self>) {
        if !self.updater.ready() || self.update_blocked(cx) {
            return;
        }
        if let Err(error) = self.save_session(cx) {
            self.updater.save_error = Some(format!("Couldn't save your session: {error:#}"));
            cx.notify();
            return;
        }
        self.updater.save_error = None;
        #[cfg(all(feature = "auto-updates", not(test)))]
        unsafe {
            native::gander_updater_install()
        };
        self.updater.state = 2;
        cx.notify();
    }

    pub(crate) fn render_update_button(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.updater.state == 0 && self.updater.error.is_none() {
            return div().into_any_element();
        }
        let blocked = self.update_blocked(cx);
        let ready = self.updater.ready();
        let error = self
            .updater
            .save_error
            .clone()
            .or_else(|| self.updater.error.clone());
        let tooltip = error.clone().unwrap_or_else(|| {
            if blocked {
                "Finish pending writes/uploads or resolve unverified comments before restarting"
                    .into()
            } else {
                "Install the downloaded update and restore this session".into()
            }
        });
        let button = Button::new("restart-to-update")
            .small()
            .ghost()
            .label(if self.updater.state == 2 {
                "Updating…"
            } else if ready {
                "Restart to update"
            } else {
                "Update failed"
            })
            .tooltip(tooltip)
            .disabled(self.updater.state == 2 || (ready && blocked))
            .on_click(cx.listener(|app, _, _, cx| {
                if app.updater.ready() {
                    app.install_update(cx);
                } else {
                    #[cfg(all(feature = "auto-updates", not(test)))]
                    unsafe {
                        native::gander_updater_retry()
                    };
                }
            }));
        div()
            .debug_selector(|| "update-button".into())
            .child(button)
            .into_any_element()
    }
}
