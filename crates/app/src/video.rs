//! One playback session shared by all Markdown hosts and the media overlay.
//! Fixed-size cards keep virtualized diff geometry independent of decoding.
use crate::{
    http::video::{self as download, Transfer, VideoFile},
    video_player::VideoPlayer,
    ReviewApp,
};
use gpui::component::{button::Button, Sizable};
use gpui::{
    div, prelude::*, px, App, Context, Entity, MouseButton, SharedString, Task, WeakEntity, Window,
};
use std::{
    collections::HashMap,
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct VideoTarget {
    pub url: SharedString,
    pub label: SharedString,
    // HTML <video> is explicit; extensionless links require MIME discovery.
    pub explicit: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Origin {
    pub item: u64,
    pub pane: crate::ItemPane,
    pub key: SharedString,
}

#[derive(Clone)]
enum Discovery {
    Pending,
    Video,
    Other,
    Failed,
}

pub(crate) type VideoDownload = Arc<
    dyn Fn(String, Arc<Transfer>) -> futures::future::BoxFuture<'static, anyhow::Result<VideoFile>>
        + Send
        + Sync,
>;

pub(crate) type VideoProbe =
    Arc<dyn Fn(String) -> futures::future::BoxFuture<'static, anyhow::Result<bool>> + Send + Sync>;

pub(crate) struct VideoHub {
    metadata: HashMap<SharedString, Discovery>,
    pub session: Option<Entity<VideoSession>>,
    pub expanded: bool,
    pub download: VideoDownload,
    pub probe: VideoProbe,
}

impl Default for VideoHub {
    fn default() -> Self {
        Self {
            metadata: HashMap::new(),
            session: None,
            expanded: false,
            probe: Arc::new(|url| {
                Box::pin(async move {
                    download::discover(url)
                        .await
                        .map_err(|_| anyhow::anyhow!("Attachment metadata worker stopped"))?
                })
            }),
            download: Arc::new(|url, transfer| {
                Box::pin(async move {
                    download::download(url, transfer)
                        .await
                        .map_err(|_| anyhow::anyhow!("Video download worker stopped"))?
                })
            }),
        }
    }
}

impl VideoHub {
    pub fn sync(
        &mut self,
        item_ids: &[u64],
        active: Option<(u64, crate::ItemPane)>,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = &self.session else {
            return;
        };
        let origin = session.read(cx).origin.clone();
        if !item_ids.contains(&origin.item) {
            session.update(cx, |s, _| s.cancel());
            self.session = None;
            self.expanded = false;
        } else if active != Some((origin.item, origin.pane)) {
            session.update(cx, |s, _| s.pause());
        }
    }

    pub fn non_video(&self, url: &str) -> bool {
        matches!(self.metadata.get(url), Some(Discovery::Other))
    }

    pub fn pause(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            session.update(cx, |s, _| s.pause());
        }
    }

    pub fn release(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = self.session.take() {
            session.update(cx, |s, _| s.cancel());
        }
        self.expanded = false;
    }

    fn discover(&mut self, target: &VideoTarget, cx: &mut Context<Self>) -> Discovery {
        if target.explicit || download::direct_video(&target.url) {
            return Discovery::Video;
        }
        if let Some(state) = self.metadata.get(&target.url) {
            return state.clone();
        }
        // Bound retained discovery state. No body is downloaded by discovery.
        if self.metadata.len() >= 256 {
            return Discovery::Failed;
        }
        if self
            .metadata
            .values()
            .filter(|s| matches!(s, Discovery::Pending))
            .count()
            >= 4
        {
            return Discovery::Pending;
        }
        self.metadata.insert(target.url.clone(), Discovery::Pending);
        let url = target.url.clone();
        let rx = (self.probe)(url.to_string());
        let response = cx.background_executor().spawn(async move { rx.await });
        cx.spawn(async move |this, cx| {
            let state = match response.await {
                Ok(true) => Discovery::Video,
                Ok(false) => Discovery::Other,
                _ => Discovery::Failed,
            };
            let _ = this.update(cx, |hub, cx| {
                hub.metadata.insert(url, state);
                cx.notify();
            });
        })
        .detach();
        Discovery::Pending
    }

    fn start(
        &mut self,
        target: VideoTarget,
        origin: Origin,
        owner: WeakEntity<ReviewApp>,
        cx: &mut Context<Self>,
    ) {
        self.release(cx);
        self.session =
            Some(cx.new(|cx| VideoSession::new(target, origin, owner, self.download.clone(), cx)));
        cx.notify();
    }
}

pub(crate) fn card(
    target: VideoTarget,
    source: SharedString,
    origin: Origin,
    hub: Entity<VideoHub>,
    owner: WeakEntity<ReviewApp>,
    cx: &mut App,
) -> gpui::AnyElement {
    let state = hub.update(cx, |hub, cx| hub.discover(&target, cx));
    let active = hub
        .read(cx)
        .session
        .as_ref()
        .filter(|s| s.read(cx).origin == origin)
        .cloned();
    let expanded = hub.read(cx).expanded;
    if let Some(session) = active.clone() {
        if !expanded {
            return div().w_full().h(px(260.)).child(session).into_any_element();
        }
    }
    let url = target.url.clone();
    let label = if target.label.is_empty() {
        target.url.clone()
    } else {
        target.label.clone()
    };
    // Non-video attachment links remain ordinary links and never GET a body.
    if matches!(state, Discovery::Other) {
        return gpui::component::text::TextView::markdown(
            SharedString::from(format!("{}-attachment-link", origin.key)),
            source,
        )
        .w_auto()
        .h_auto()
        .scrollable(false)
        .selectable(true)
        .into_any_element();
    }
    div()
        .id("video-card")
        .w_full()
        .h(px(260.))
        .overflow_hidden()
        .flex()
        .flex_col()
        .justify_center()
        .items_center()
        .gap_2()
        .bg(gpui::rgb(0x16181d))
        .text_color(gpui::rgb(0xeeeeee))
        .debug_selector(|| "video-card".into())
        .on_mouse_down(MouseButton::Left, |_, window, cx| {
            window.prevent_default();
            cx.stop_propagation();
        })
        .on_click(|_, _, cx| cx.stop_propagation())
        .child(div().text_sm().child(label))
        .child(match state {
            Discovery::Pending => div().child("Checking attachment type…").into_any_element(),
            Discovery::Failed => {
                let hub = hub.clone();
                let url = target.url.clone();
                div()
                    .child("Could not check attachment type. ")
                    .child(
                        Button::new("video-discovery-retry")
                            .label("Retry")
                            .small()
                            .on_click(move |_, _, cx| {
                                hub.update(cx, |hub, cx| {
                                    hub.metadata.remove(&url);
                                    cx.notify();
                                });
                            }),
                    )
                    .into_any_element()
            }
            _ if expanded && active.is_some() => {
                div().child("Playing in viewer").into_any_element()
            }
            _ => div()
                .child(control(
                    "video-play",
                    Button::new("video-play")
                        .label("Play video")
                        .small()
                        .on_click(move |_, _, cx| {
                            hub.update(cx, |hub, cx| {
                                hub.start(target.clone(), origin.clone(), owner.clone(), cx)
                            });
                        }),
                ))
                .into_any_element(),
        })
        .child(
            Button::new("video-browser")
                .label("Open in browser")
                .small()
                .on_click(move |_, _, cx| cx.open_url(&url)),
        )
        .into_any_element()
}

pub(crate) struct VideoSession {
    pub target: VideoTarget,
    pub origin: Origin,
    owner: WeakEntity<ReviewApp>,
    download: VideoDownload,
    player: Option<VideoPlayer>,
    // Declared after player so the decoder releases the file before deletion.
    file: Option<VideoFile>,
    transfer: Arc<Transfer>,
    error: Option<SharedString>,
    canceled: bool,
    ready_since: Instant,
    play_when_ready: bool,
    download_task: Option<Task<()>>,
    tick_task: Option<Task<()>>,
}

impl VideoSession {
    fn new(
        target: VideoTarget,
        origin: Origin,
        owner: WeakEntity<ReviewApp>,
        download: VideoDownload,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut session = Self {
            target,
            origin,
            owner,
            download,
            player: None,
            file: None,
            transfer: Arc::default(),
            error: None,
            canceled: false,
            ready_since: Instant::now(),
            play_when_ready: true,
            download_task: None,
            tick_task: None,
        };
        session.load(cx);
        session
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        self.cancel();
        self.canceled = false;
        self.play_when_ready = true;
        self.error = None;
        self.transfer = Arc::default();
        let rx = (self.download)(self.target.url.to_string(), self.transfer.clone());
        // Bridge worker-thread completion through GPUI's background scheduler;
        // never wake a foreground-local task directly from a foreign thread.
        let response = cx.background_executor().spawn(async move { rx.await });
        self.download_task = Some(cx.spawn(async move |this, cx| {
            let result = response.await;
            let _ = this.update(cx, |s, cx| {
                if s.canceled {
                    return;
                }
                match result {
                    Ok(file) => match VideoPlayer::new(&file.path) {
                        Ok(mut player) => {
                            if s.play_when_ready {
                                player.play();
                            }
                            s.player = Some(player);
                            s.file = Some(file);
                            s.ready_since = Instant::now();
                        }
                        Err(_) => s.error = Some("Could not initialize video playback".into()),
                    },
                    Err(error) => s.error = Some(error.to_string().into()),
                }
                cx.notify();
            });
        }));
        self.tick_task = Some(cx.spawn(async move |this, cx| {
            let mut interval = Duration::from_millis(16);
            let mut previous = None;
            loop {
                cx.background_executor().timer(interval).await;
                let result = this.update(cx, |s, cx| {
                    if s.canceled || s.error.is_some() {
                        return false;
                    }
                    let old_frame = s.player.as_ref().and_then(|p| p.frame.clone());
                    if let Some(player) = &mut s.player {
                        if let Err(error) = player.tick() {
                            s.error = Some(error.to_string().into());
                            player.pause();
                        } else if (!player.ready() || (player.playing() && player.frame.is_none()))
                            && s.ready_since.elapsed() > Duration::from_secs(20)
                        {
                            player.pause();
                            s.error =
                                Some("Unsupported format or video preparation timed out".into());
                        }
                    }
                    let state = (
                        s.transfer.bytes.load(Ordering::Relaxed),
                        s.player
                            .as_ref()
                            .map(|p| (p.ready(), p.playing(), (p.position() * 10.) as u64)),
                        s.error.clone(),
                    );
                    if previous.as_ref() != Some(&state)
                        || old_frame != s.player.as_ref().and_then(|p| p.frame.clone())
                    {
                        cx.notify();
                    }
                    previous = Some(state);
                    interval = Duration::from_millis(
                        if s.player.as_ref().is_some_and(|p| p.playing() || !p.ready()) {
                            16
                        } else {
                            250
                        },
                    );
                    true
                });
                if !matches!(result, Ok(true)) {
                    break;
                }
            }
        }));
    }

    pub fn is_playing(&self) -> bool {
        self.play_when_ready
            && (self
                .player
                .as_ref()
                .is_none_or(|p| !p.ready() || p.playing()))
    }
    pub fn pause(&mut self) {
        self.play_when_ready = false;
        if let Some(player) = &mut self.player {
            player.pause();
        }
    }
    pub fn cancel(&mut self) {
        self.transfer.cancel();
        self.pause();
        self.player = None;
        self.file = None;
        self.download_task = None;
        self.tick_task = None;
        self.canceled = true;
    }
    pub fn key(&mut self, key: &str, cx: &mut Context<Self>) {
        if self.canceled || self.error.is_some() {
            return;
        }
        if key == "space" {
            let play = !self.is_playing();
            self.play_when_ready = play;
            if let Some(player) = &mut self.player {
                if play {
                    self.ready_since = Instant::now();
                    player.play();
                } else {
                    player.pause();
                }
            }
            cx.notify();
            return;
        }
        if let Some(player) = &mut self.player {
            match key {
                "left" => player.seek(player.position() - 5.),
                "right" => player.seek(player.position() + 5.),
                "m" => player.set_muted(!player.muted()),
                _ => {}
            }
            cx.notify();
        }
    }
}
impl Drop for VideoSession {
    fn drop(&mut self) {
        self.transfer.cancel();
    }
}

fn control(id: &'static str, button: Button) -> gpui::AnyElement {
    div()
        .debug_selector(move || id.into())
        .child(button)
        .into_any_element()
}

fn clock(seconds: f64) -> String {
    let n = seconds as u64;
    format!("{}:{:02}", n / 60, n % 60)
}

impl Render for VideoSession {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let expanded = self
            .owner
            .upgrade()
            .is_some_and(|owner| owner.read(cx).video_hub.read(cx).expanded);
        let frame = self.player.as_ref().and_then(|p| p.frame.clone());
        let ready = self.player.as_ref().is_some_and(|p| p.ready());
        let playing = self.is_playing();
        let position = self.player.as_ref().map_or(0., |p| p.position());
        let duration = self.player.as_ref().map_or(0., |p| p.duration());
        let muted = self.player.as_ref().is_some_and(|p| p.muted());
        let volume = self.player.as_ref().map_or(1., |p| p.volume());
        let bytes = self.transfer.bytes.load(Ordering::Relaxed);
        let total = self.transfer.total.load(Ordering::Relaxed);
        let status = if let Some(error) = &self.error {
            error.to_string()
        } else if self.canceled {
            "Download canceled".to_owned()
        } else if self.player.is_some() && !ready {
            "Preparing video…".to_owned()
        } else if self.player.is_some() {
            String::new()
        } else if total > 0 {
            format!(
                "Downloading video… {}% ({:.1} MiB)",
                bytes * 100 / total,
                bytes as f64 / 1048576.
            )
        } else {
            format!("Downloading video… {:.1} MiB", bytes as f64 / 1048576.)
        };
        let status_selector = if self.error.is_some() {
            "video-error"
        } else if self.canceled {
            "video-canceled"
        } else {
            "video-loading"
        };
        let owner = self.owner.clone();
        let target = self.target.clone();
        let origin = self.origin.clone();
        let url = self.target.url.clone();
        let seek_bounds =
            std::rc::Rc::new(std::cell::Cell::new(None::<gpui::Bounds<gpui::Pixels>>));
        let record_bounds = seek_bounds.clone();
        div()
            .id("video-session")
            .debug_selector(|| "video-session".into())
            .size_full()
            .overflow_hidden()
            .flex()
            .flex_col()
            .bg(gpui::rgb(0x101114))
            .text_color(gpui::rgb(0xeeeeee))
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .flex()
                    .items_center()
                    .justify_center()
                    .children(
                        frame
                            .filter(|_| status.is_empty())
                            .map(|frame| gpui::surface(frame).size_full()),
                    )
                    .when(!status.is_empty(), |element| {
                        element.child(
                            div()
                                .debug_selector(move || status_selector.into())
                                .text_sm()
                                .px_3()
                                .child(status),
                        )
                    }),
            )
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(px(8.))
                    .bg(gpui::rgb(0x44464c))
                    .cursor_pointer()
                    .debug_selector(|| "video-seek".into())
                    .child(
                        div()
                            .h_full()
                            .w(gpui::relative(if duration > 0. {
                                (position / duration) as f32
                            } else {
                                0.
                            }))
                            .bg(gpui::rgb(0x89b4fa)),
                    )
                    .child(
                        gpui::canvas(
                            move |bounds, _, _| {
                                record_bounds.set(Some(bounds));
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |s, event: &gpui::MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            if let (Some(bounds), Some(player)) = (seek_bounds.get(), &s.player) {
                                let fraction = f32::from(event.position.x - bounds.left())
                                    / f32::from(bounds.size.width).max(1.);
                                player.seek(player.duration() * fraction.clamp(0., 1.) as f64);
                                cx.notify();
                            }
                        }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .p_1()
                    .when(ready && self.error.is_none(), |bar| {
                        bar.child(
                            Button::new("video-toggle")
                                .label(if playing { "Pause" } else { "Play" })
                                .small()
                                .on_click(cx.listener(|s, _, _, cx| s.key("space", cx))),
                        )
                        .child(
                            Button::new("video-back")
                                .label("−5s")
                                .small()
                                .on_click(cx.listener(|s, _, _, cx| s.key("left", cx))),
                        )
                        .child(div().text_xs().child(format!(
                            "{} / {}",
                            clock(position),
                            clock(duration)
                        )))
                        .child(
                            Button::new("video-forward")
                                .label("+5s")
                                .small()
                                .on_click(cx.listener(|s, _, _, cx| s.key("right", cx))),
                        )
                        .child(
                            Button::new("video-mute")
                                .label(if muted { "Unmute" } else { "Mute" })
                                .small()
                                .on_click(cx.listener(|s, _, _, cx| s.key("m", cx))),
                        )
                        .child(
                            Button::new("video-volume-down")
                                .label("Vol −")
                                .small()
                                .on_click(cx.listener(|s, _, _, cx| {
                                    if let Some(p) = &s.player {
                                        p.set_volume(p.volume() - 0.1);
                                    }
                                    cx.notify();
                                })),
                        )
                        .child(div().text_xs().child(format!("{:.0}%", volume * 100.)))
                        .child(
                            Button::new("video-volume-up")
                                .label("Vol +")
                                .small()
                                .on_click(cx.listener(|s, _, _, cx| {
                                    if let Some(p) = &s.player {
                                        p.set_volume(p.volume() + 0.1);
                                    }
                                    cx.notify();
                                })),
                        )
                    })
                    .when(!ready && !self.canceled && self.error.is_none(), |bar| {
                        bar.child(control(
                            "video-cancel",
                            Button::new("video-cancel")
                                .label("Cancel")
                                .small()
                                .on_click(cx.listener(|s, _, _, cx| {
                                    s.cancel();
                                    cx.notify();
                                })),
                        ))
                    })
                    .when(self.error.is_some() || self.canceled, |bar| {
                        bar.child(control(
                            "video-retry",
                            Button::new("video-retry")
                                .label("Retry")
                                .small()
                                .on_click(cx.listener(|s, _, _, cx| s.load(cx))),
                        ))
                    })
                    .when(!expanded, |bar| {
                        bar.child(control(
                            "video-expand",
                            Button::new("video-expand")
                                .label("Expand")
                                .small()
                                .on_click(move |_, window, cx| {
                                    let _ = owner.update(cx, |app, cx| {
                                        app.open_video(target.clone(), origin.clone(), window, cx)
                                    });
                                }),
                        ))
                    })
                    .child(
                        Button::new("video-browser")
                            .label("Browser")
                            .small()
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    ),
            )
    }
}
