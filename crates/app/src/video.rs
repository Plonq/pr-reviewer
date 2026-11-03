//! One playback session shared by all Markdown hosts and the media overlay.
//! Fixed-size cards keep virtualized diff geometry independent of decoding.
use crate::{
    http::video::{self as download, Transfer, VideoFile},
    video_player::VideoPlayer,
    ReviewApp,
};
use gpui::component::{
    button::{Button, ButtonVariants},
    Sizable,
};
use gpui_kit_assets::IconName;
pub(crate) const INLINE_HEIGHT: f32 = 420.;
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

// Preparation completes after the initial range, not after the full download.
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
    previews: Vec<(Origin, Entity<VideoSession>)>,
    pub expanded: bool,
    pub download: VideoDownload,
    pub probe: VideoProbe,
}

impl Default for VideoHub {
    fn default() -> Self {
        Self {
            metadata: HashMap::new(),
            session: None,
            previews: Vec::new(),
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
        let playing = self.session.as_ref().map(|s| s.entity_id());
        self.previews.retain(|(origin, session)| {
            let in_pane = active == Some((origin.item, origin.pane));
            let last_played = playing == Some(session.entity_id());
            if item_ids.contains(&origin.item) && (in_pane || last_played) {
                true
            } else {
                session.update(cx, |s, _| s.cancel());
                false
            }
        });
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
        for (_, session) in self.previews.drain(..) {
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
        if let Some(old) = &self.session {
            old.update(cx, |s, _| s.pause());
        }
        if let Some(session) = self.preload(target, origin, owner, true, cx) {
            session.update(cx, |s, cx| {
                s.activated = true;
                s.resume(cx);
            });
            self.session = Some(session);
        }
        cx.notify();
    }

    fn preload(
        &mut self,
        target: VideoTarget,
        origin: Origin,
        owner: WeakEntity<ReviewApp>,
        force: bool,
        cx: &mut Context<Self>,
    ) -> Option<Entity<VideoSession>> {
        if let Some((_, session)) = self.previews.iter().find(|(key, _)| key == &origin) {
            return Some(session.clone());
        }
        // Keep at most four bounded media caches/decoders. Additional cards can
        // preload on hover or Play, without render-time eviction/reload loops.
        if self.previews.len() >= 4 {
            if !force {
                return None;
            }
            let index = self.previews.iter().position(|(_, s)| {
                self.session
                    .as_ref()
                    .is_none_or(|active| active.entity_id() != s.entity_id())
            })?;
            let (_, old) = self.previews.remove(index);
            old.update(cx, |s, _| s.cancel());
        }
        let hub = cx.entity().downgrade();
        let session = cx.new(|cx| {
            VideoSession::new(
                target,
                origin.clone(),
                owner,
                hub,
                self.download.clone(),
                cx,
            )
        });
        self.previews.push((origin, session.clone()));
        Some(session)
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
    if matches!(state, Discovery::Video) && !(expanded && active.is_some()) {
        if let Some(session) = hub.update(cx, |hub, cx| {
            hub.preload(target.clone(), origin.clone(), owner.clone(), false, cx)
        }) {
            return div()
                .w_full()
                .h(px(INLINE_HEIGHT))
                .debug_selector(|| "video-card".into())
                .child(session)
                .into_any_element();
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
    let hover_hub = hub.clone();
    let hover_target = target.clone();
    let hover_origin = origin.clone();
    let hover_owner = owner.clone();
    let can_preload = matches!(state, Discovery::Video) && !expanded;
    div()
        .id("video-card")
        .relative()
        .on_hover(move |hovered, _, cx| {
            if *hovered && can_preload {
                hover_hub.update(cx, |hub, cx| {
                    hub.preload(
                        hover_target.clone(),
                        hover_origin.clone(),
                        hover_owner.clone(),
                        true,
                        cx,
                    );
                    cx.notify();
                });
            }
        })
        .w_full()
        .h(px(INLINE_HEIGHT))
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
                            .icon(IconName::RotateCcw)
                            .tooltip("Retry attachment discovery")
                            .ghost()
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
                        .icon(IconName::Play)
                        .tooltip("Play video")
                        .ghost()
                        .size(px(48.))
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
                .icon(IconName::ExternalLink)
                .tooltip("Open in browser")
                .ghost()
                .small()
                .absolute()
                .bottom_3()
                .right_3()
                .on_click(move |_, _, cx| cx.open_url(&url)),
        )
        .into_any_element()
}

pub(crate) struct VideoSession {
    pub target: VideoTarget,
    pub origin: Origin,
    owner: WeakEntity<ReviewApp>,
    hub: WeakEntity<VideoHub>,
    download: VideoDownload,
    player: Option<VideoPlayer>,
    // Declared after player so the decoder releases the file before deletion.
    file: Option<VideoFile>,
    transfer: Arc<Transfer>,
    error: Option<SharedString>,
    canceled: bool,
    ready_since: Instant,
    play_when_ready: bool,
    activated: bool,
    download_task: Option<Task<()>>,
    tick_task: Option<Task<()>>,
}

impl VideoSession {
    fn new(
        target: VideoTarget,
        origin: Origin,
        owner: WeakEntity<ReviewApp>,
        hub: WeakEntity<VideoHub>,
        download: VideoDownload,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut session = Self {
            target,
            origin,
            owner,
            hub,
            download,
            player: None,
            file: None,
            transfer: Arc::default(),
            error: None,
            canceled: false,
            ready_since: Instant::now(),
            play_when_ready: false,
            activated: false,
            download_task: None,
            tick_task: None,
        };
        session.load(cx);
        session
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        self.cancel();
        self.canceled = false;
        self.play_when_ready = self.activated;
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
                    Ok(file) => match if s.transfer.complete.load(Ordering::Acquire) {
                        VideoPlayer::new(&file.path)
                    } else {
                        VideoPlayer::stream(file.source())
                    } {
                        Ok(mut player) => {
                            if s.activated {
                                file.start();
                            }
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
                s.start_tick(cx);
                cx.notify();
            });
        }));
        self.start_tick(cx);
    }

    fn start_tick(&mut self, cx: &mut Context<Self>) {
        self.tick_task = Some(cx.spawn(async move |this, cx| {
            let mut interval = Duration::from_millis(16);
            let mut previous = None;
            loop {
                cx.background_executor().timer(interval).await;
                let result = this.update(cx, |s, cx| {
                    if s.canceled || s.error.is_some() {
                        return false;
                    }
                    let download_error = s.transfer.error.lock().unwrap().clone();
                    if let Some(error) = download_error {
                        s.error = Some(error.into());
                        if let Some(player) = &mut s.player {
                            player.pause();
                        }
                    }
                    let old_frame = s.player.as_ref().and_then(|p| p.frame.clone());
                    if let Some(player) = &mut s.player {
                        if let Err(error) = player.tick() {
                            s.error = Some(error.to_string().into());
                            player.pause();
                            s.transfer.cancel();
                        } else if s.activated
                            && (!player.ready() || (player.playing() && player.frame.is_none()))
                            && s.ready_since.elapsed() > Duration::from_secs(20)
                        {
                            player.pause();
                            s.error =
                                Some("Unsupported format or video preparation timed out".into());
                            s.transfer.cancel();
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

    fn resume(&mut self, cx: &mut Context<Self>) {
        self.play_when_ready = true;
        self.ready_since = Instant::now();
        if let Some(file) = &self.file {
            file.start();
        }
        if let Some(player) = &mut self.player {
            player.play();
        }
        self.start_tick(cx);
        cx.notify();
    }
    fn activate(&mut self, cx: &mut Context<Self>) {
        self.activated = true;
        if let Some(hub) = self.hub.upgrade() {
            let entity = cx.entity();
            hub.update(cx, |hub, cx| {
                if let Some(old) = &hub.session {
                    if old.entity_id() != entity.entity_id() {
                        old.update(cx, |s, _| s.pause());
                    }
                }
                hub.session = Some(entity);
                cx.notify();
            });
        }
        self.resume(cx);
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
            if !self.activated {
                self.activate(cx);
                return;
            }
            if self.is_playing() {
                self.pause();
                cx.notify();
            } else {
                self.resume(cx);
            }
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
        self.render_player(cx)
    }
}
#[path = "video/controls.rs"]
mod controls;
