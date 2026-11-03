//! Video chrome occupies no layout space: the picture keeps the whole card.
use super::*;

fn icon(id: &'static str, name: IconName, tip: &'static str) -> Button {
    Button::new(id)
        .icon(name)
        .tooltip(tip)
        .ghost()
        .small()
        .text_color(gpui::rgb(0xffffff))
}
fn slider(
    id: &'static str,
    value: f32,
    buffered: f32,
    cx: &mut Context<VideoSession>,
    change: fn(&mut VideoSession, f32, &mut Context<VideoSession>),
) -> gpui::AnyElement {
    let bounds = std::rc::Rc::new(std::cell::Cell::new(None::<gpui::Bounds<gpui::Pixels>>));
    let record = bounds.clone();
    div()
        .id(id)
        .debug_selector(move || id.into())
        .relative()
        .w_full()
        .h(px(12.))
        .cursor_pointer()
        .child(
            div()
                .absolute()
                .top(px(4.))
                .h(px(4.))
                .w_full()
                .rounded_full()
                .bg(gpui::rgba(0xffffff40)),
        )
        .child(
            div()
                .absolute()
                .top(px(4.))
                .h(px(4.))
                .w(gpui::relative(buffered.clamp(0., 1.)))
                .rounded_full()
                .bg(gpui::rgba(0xffffff60)),
        )
        .child(
            div()
                .absolute()
                .top(px(4.))
                .h(px(4.))
                .w(gpui::relative(value.clamp(0., 1.)))
                .rounded_full()
                .bg(gpui::rgb(0xffffff)),
        )
        .child(
            gpui::canvas(move |b, _, _| record.set(Some(b)), |_, _, _, _| {})
                .absolute()
                .size_full(),
        )
        .on_click(|_, _, cx| cx.stop_propagation())
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |s, event: &gpui::MouseDownEvent, _, cx| {
                cx.stop_propagation();
                if let Some(b) = bounds.get() {
                    let v =
                        f32::from(event.position.x - b.left()) / f32::from(b.size.width).max(1.);
                    change(s, v.clamp(0., 1.), cx);
                }
            }),
        )
        .into_any_element()
}
impl VideoSession {
    pub(super) fn render_player(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let expanded = self.hub.upgrade().is_some_and(|hub| hub.read(cx).expanded);
        let frame = self.player.as_ref().and_then(|p| p.frame.clone());
        let playing = self.is_playing();
        let position = self.player.as_ref().map_or(0., |p| p.position());
        let duration = self.player.as_ref().map_or(0., |p| p.duration());
        let muted = self.player.as_ref().is_some_and(|p| p.muted());
        let volume = self.player.as_ref().map_or(1., |p| p.volume());
        let bytes = self.transfer.bytes.load(Ordering::Relaxed);
        let total = self.transfer.total.load(Ordering::Relaxed);
        let complete = self.transfer.complete.load(Ordering::Acquire);
        let failed = self.error.is_some() || self.canceled;
        let status = if let Some(error) = &self.error {
            error.to_string()
        } else if self.canceled {
            "Download canceled".into()
        } else if frame.is_none() {
            if self.activated {
                "Buffering…".into()
            } else {
                "Loading preview…".into()
            }
        } else {
            String::new()
        };
        let status_id = if self.error.is_some() {
            "video-error"
        } else if self.canceled {
            "video-canceled"
        } else {
            "video-loading"
        };
        let session = cx.entity().downgrade();
        let owner = self.owner.clone();
        let target = self.target.clone();
        let origin = self.origin.clone();
        let url = self.target.url.clone();
        let seek = slider(
            "video-seek",
            if duration > 0. {
                (position / duration) as f32
            } else {
                0.
            },
            if total > 0 {
                bytes as f32 / total as f32
            } else {
                0.
            },
            cx,
            |s, v, cx| {
                if let Some(p) = &s.player {
                    p.seek(p.duration() * v as f64);
                }
                cx.notify();
            },
        );
        let volume_slider = slider("video-volume", volume, 0., cx, |s, v, cx| {
            if let Some(p) = &s.player {
                p.set_volume(v);
                p.set_muted(v == 0.);
            }
            cx.notify();
        });
        div()
            .id("video-session")
            .debug_selector(|| "video-session".into())
            .group("video")
            .relative()
            .size_full()
            .overflow_hidden()
            .rounded_lg()
            .bg(gpui::rgb(0x08090b))
            .text_color(gpui::rgb(0xffffff))
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .on_click(cx.listener(|s, _, _, cx| {
                cx.stop_propagation();
                s.key("space", cx);
            }))
            .children(frame.map(|f| gpui::surface(f).size_full()))
            .when(!status.is_empty(), |root| {
                root.child(
                    div()
                        .absolute()
                        .inset_0()
                        .pb(px(96.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .debug_selector(move || status_id.into())
                                .max_w_full()
                                .p_4()
                                .text_sm()
                                .child(status),
                        ),
                )
            })
            .when(!playing && !failed, |root| {
                root.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(control(
                            "video-play",
                            icon("video-play", IconName::Play, "Play video")
                                .icon(gpui::component::Icon::new(IconName::Play).size(px(28.)))
                                .size(px(56.))
                                .rounded_full()
                                .bg(gpui::rgba(0x000000a0))
                                .on_click(cx.listener(|s, _, _, cx| {
                                    cx.stop_propagation();
                                    s.key("space", cx);
                                })),
                        )),
                )
            })
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .p_3()
                    .text_xs()
                    .bg(gpui::rgba(0x00000070))
                    .when(playing, |bar| {
                        bar.opacity(0.).group_hover("video", |bar| bar.opacity(1.))
                    })
                    .child(self.target.label.clone()),
            )
            .child(
                div()
                    .id("video-controls")
                    .debug_selector(|| "video-controls".into())
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .px_3()
                    .py_2()
                    .bg(gpui::rgba(0x000000b0))
                    .when(playing, |bar| {
                        bar.opacity(0.).group_hover("video", |bar| bar.opacity(1.))
                    })
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(seek)
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_1()
                            .when(!failed, |bar| {
                                bar.child(control(
                                    "video-toggle",
                                    icon(
                                        "video-toggle",
                                        if playing {
                                            IconName::Pause
                                        } else {
                                            IconName::Play
                                        },
                                        if playing { "Pause" } else { "Play" },
                                    )
                                    .on_click(cx.listener(
                                        |s, _, _, cx| {
                                            cx.stop_propagation();
                                            s.key("space", cx);
                                        },
                                    )),
                                ))
                            })
                            .when(failed, |bar| {
                                bar.child(control(
                                    "video-retry",
                                    icon("video-retry", IconName::RotateCcw, "Retry").on_click(
                                        cx.listener(|s, _, _, cx| {
                                            cx.stop_propagation();
                                            s.load(cx);
                                        }),
                                    ),
                                ))
                            })
                            .child(div().text_xs().child(format!(
                                "{} / {}",
                                clock(position),
                                clock(duration)
                            )))
                            .child(
                                icon(
                                    "video-mute",
                                    if muted {
                                        IconName::VolumeX
                                    } else {
                                        IconName::Volume2
                                    },
                                    if muted { "Unmute" } else { "Mute" },
                                )
                                .on_click(cx.listener(
                                    |s, _, _, cx| {
                                        cx.stop_propagation();
                                        s.key("m", cx);
                                    },
                                )),
                            )
                            .child(div().w(px(64.)).flex_shrink_0().child(volume_slider))
                            .child(div().flex_1())
                            .when(!complete && !failed, |bar| {
                                bar.child(div().text_xs().child(if total > 0 {
                                    format!("{}%", bytes * 100 / total)
                                } else {
                                    String::new()
                                }))
                                .child(control(
                                    "video-cancel",
                                    icon("video-cancel", IconName::X, "Cancel download").on_click(
                                        cx.listener(|s, _, _, cx| {
                                            cx.stop_propagation();
                                            s.cancel();
                                            cx.notify();
                                        }),
                                    ),
                                ))
                            })
                            .child(
                                icon(
                                    "video-browser",
                                    IconName::ExternalLink,
                                    "Open video in browser",
                                )
                                .on_click(move |_, _, cx| {
                                    cx.stop_propagation();
                                    cx.open_url(&url);
                                }),
                            )
                            .child(control(
                                "video-expand",
                                icon(
                                    "video-expand",
                                    if expanded {
                                        IconName::Minimize2
                                    } else {
                                        IconName::Maximize2
                                    },
                                    if expanded {
                                        "Return inline (Esc)"
                                    } else {
                                        "Expand video"
                                    },
                                )
                                .on_click(move |_, window, cx| {
                                    cx.stop_propagation();
                                    let Some(session) = session.upgrade() else {
                                        return;
                                    };
                                    let _ = owner.update(cx, |app, cx| {
                                        if expanded {
                                            app.close_image(window, cx);
                                        } else {
                                            app.video_hub.update(cx, |hub, cx| {
                                                if let Some(old) = &hub.session {
                                                    if old.entity_id() != session.entity_id() {
                                                        old.update(cx, |s, _| s.pause());
                                                    }
                                                }
                                                hub.session = Some(session.clone());
                                                cx.notify();
                                            });
                                            app.open_video(
                                                target.clone(),
                                                origin.clone(),
                                                window,
                                                cx,
                                            );
                                        }
                                    });
                                }),
                            )),
                    ),
            )
            .into_any_element()
    }
}
