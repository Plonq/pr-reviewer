//! Sparkle owns downloading, verification and installation. GPUI owns the only
//! update UI. Development builds do not link or run Sparkle.
use crate::{ItemState, Quit, ReviewApp};
use gpui::component::{
    button::{Button, ButtonVariants},
    Disableable, Sizable,
};
use gpui::{div, prelude::*, App, Context, Entity, Menu, MenuItem};

gpui::actions!(gander, [CheckForUpdates]);

pub(crate) fn app_menu(updates_enabled: bool) -> Menu {
    Menu::new("Gander").items([
        MenuItem::action("Check for Updates…", CheckForUpdates).disabled(!updates_enabled),
        MenuItem::separator(),
        MenuItem::action("Quit Gander", Quit),
    ])
}

/// A global action also works while an editor or a modal owns keyboard focus.
pub(crate) fn init_menu(view: &Entity<ReviewApp>, cx: &mut App) {
    let view = view.downgrade();
    cx.on_action(move |_: &CheckForUpdates, cx| {
        let _ = view.update(cx, |app, cx| app.check_for_updates(cx));
    });
    cx.set_menus([app_menu(cfg!(any(feature = "auto-updates", test)))]);
}

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
        pub fn gander_updater_check();
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
                        app.set_update_status(state, error, cx);
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

    #[cfg(any(feature = "auto-updates", test))]
    pub(crate) fn set_update_status(
        &mut self,
        state: i32,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.updater.state != state || self.updater.error != error {
            self.updater.state = state;
            self.updater.error = error;
            cx.notify();
        }
    }

    pub(crate) fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        if self.updater.ready() || matches!(self.updater.state, 2 | 4 | 6) {
            return;
        }
        #[cfg(all(feature = "auto-updates", not(test)))]
        unsafe {
            native::gander_updater_check();
        }
        #[cfg(any(feature = "auto-updates", test))]
        self.set_update_status(4, None, cx);
        #[cfg(not(any(feature = "auto-updates", test)))]
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
        let notice = match self.updater.state {
            4 => Some("Checking for updates…"),
            5 => Some("Up to date"),
            6 => Some("Preparing update…"),
            _ => None,
        };
        if let Some(notice) = notice {
            return div()
                .debug_selector(|| "update-status".into())
                .text_color(crate::theme::overlay0(cx))
                .child(notice)
                .into_any_element();
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
                    app.check_for_updates(cx);
                }
            }));
        div()
            .debug_selector(|| "update-button".into())
            .child(button)
            .into_any_element()
    }
}
