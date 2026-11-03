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
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum SliderKind {
    Seek,
    Volume,
}

#[derive(Clone, Copy)]
pub(super) struct SliderDrag {
    kind: SliderKind,
    bounds: gpui::Bounds<gpui::Pixels>,
    value: f32,
}

fn slider(
    id: &'static str,
    value: f32,
    buffered: f32,
    cx: &mut Context<VideoSession>,
    kind: SliderKind,
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
            cx.listener(move |s, event: &gpui::MouseDownEvent, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
                if let Some(b) = bounds.get() {
                    s.slider_drag = Some(SliderDrag {
                        kind,
                        bounds: b,
                        value: 0.,
                    });
                    s.suppress_picture_click = true;
                    s.move_slider(event.position, cx);
                }
            }),
        )
        .into_any_element()
}
impl VideoSession {
    pub(crate) fn expanded_controls(&mut self, expanded: bool, cx: &mut Context<Self>) {
        self.slider_drag = None;
        self.suppress_picture_click = false;
        self.controls_task = None;
        self.controls_visible = true;
        self.controls_pointer = None;
        if expanded {
            self.show_controls(cx);
        }
        cx.notify();
    }

    fn show_controls(&mut self, cx: &mut Context<Self>) {
        if !self.controls_visible {
            self.controls_visible = true;
            cx.notify();
        }
        self.controls_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let _ = this.update(cx, |s, cx| {
                // Don't remove the slider from under an active gesture.
                if s.slider_drag.is_none() {
                    s.controls_visible = false;
                    cx.notify();
                }
            });
        }));
    }

    fn hide_controls(&mut self, cx: &mut Context<Self>) {
        self.controls_task = None;
        self.slider_drag = None;
        self.controls_pointer = None;
        if self.controls_visible {
            self.controls_visible = false;
            cx.notify();
        }
    }

    fn move_slider(&mut self, position: gpui::Point<gpui::Pixels>, cx: &mut Context<Self>) {
        let Some(drag) = &mut self.slider_drag else {
            return;
        };
        drag.value = (f32::from(position.x - drag.bounds.left())
            / f32::from(drag.bounds.size.width).max(1.))
        .clamp(0., 1.);
        if let Some(player) = &self.player {
            match drag.kind {
                SliderKind::Seek => player.seek(player.duration() * f64::from(drag.value)),
                SliderKind::Volume => {
                    player.set_volume(drag.value);
                    player.set_muted(drag.value == 0.);
                }
            }
        }
        self.show_controls(cx);
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn slider_gesture(&self) -> Option<(&'static str, f32)> {
        self.slider_drag.map(|d| {
            (
                match d.kind {
                    SliderKind::Seek => "seek",
                    SliderKind::Volume => "volume",
                },
                d.value,
            )
        })
    }

    pub(super) fn render_player(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let expanded = self.hub.upgrade().is_some_and(|hub| hub.read(cx).expanded);
        let chrome_visible = !expanded || self.controls_visible;
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
        let seek_value = self
            .slider_drag
            .filter(|d| d.kind == SliderKind::Seek)
            .map(|d| d.value);
        let position = seek_value.map_or(position, |v| f64::from(v) * duration);
        let volume = self
            .slider_drag
            .filter(|d| d.kind == SliderKind::Volume)
            .map_or(volume, |d| d.value);
        let seek = slider(
            "video-seek",
            seek_value.unwrap_or(if duration > 0. {
                (position / duration) as f32
            } else {
                0.
            }),
            if total > 0 {
                bytes as f32 / total as f32
            } else {
                0.
            },
            cx,
            SliderKind::Seek,
        );
        let volume_slider = slider("video-volume", volume, 0., cx, SliderKind::Volume);
        // Capture an active gesture at window scope, even outside the slider.
        let input = cx.entity().downgrade();
        let input_handlers = gpui::canvas(
            |_, _, _| {},
            move |_, _, window, _| {
                let moves = input.clone();
                window.on_mouse_event(move |event: &gpui::MouseMoveEvent, phase, _, cx| {
                    if phase != gpui::DispatchPhase::Capture {
                        return;
                    }
                    let Some(session) = moves.upgrade() else {
                        return;
                    };
                    if session.read(cx).slider_drag.is_none() {
                        return;
                    }
                    session.update(cx, |s, cx| {
                        if event.pressed_button == Some(MouseButton::Left) {
                            s.move_slider(event.position, cx);
                        } else {
                            s.slider_drag = None;
                            s.show_controls(cx);
                        }
                        cx.stop_propagation();
                    });
                });
                let releases = input.clone();
                window.on_mouse_event(move |event: &gpui::MouseUpEvent, phase, _, cx| {
                    if phase != gpui::DispatchPhase::Capture || event.button != MouseButton::Left {
                        return;
                    }
                    let Some(session) = releases.upgrade() else {
                        return;
                    };
                    if session.read(cx).slider_drag.is_none() {
                        return;
                    }
                    session.update(cx, |s, cx| {
                        s.move_slider(event.position, cx);
                        s.slider_drag = None;
                    });
                });
                let exits = input.clone();
                window.on_mouse_event(move |_: &gpui::MouseExitEvent, phase, _, cx| {
                    if phase == gpui::DispatchPhase::Capture {
                        let _ = exits.update(cx, |s, cx| s.hide_controls(cx));
                    }
                });
            },
        )
        .absolute()
        .size_full();
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
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|s, _, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                    s.suppress_picture_click = false;
                    s.show_controls(cx);
                }),
            )
            .on_click(cx.listener(|s, _, _, cx| {
                cx.stop_propagation();
                if !s.suppress_picture_click {
                    s.key("space", cx);
                }
            }))
            .when(expanded, |root| {
                root.on_mouse_move(cx.listener(|s, event: &gpui::MouseMoveEvent, _, cx| {
                    // Layout/hover refreshes can replay a stationary move.
                    if s.controls_pointer != Some(event.position) {
                        s.controls_pointer = Some(event.position);
                        s.show_controls(cx);
                    }
                }))
            })
            .children(frame.map(|f| gpui::surface(f).absolute().inset_0().size_full()))
            .child(input_handlers)
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
            .when(chrome_visible && !playing && !failed, |root| {
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
            .when(chrome_visible, |root| {
                root.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .p_3()
                        .text_xs()
                        .bg(gpui::rgba(0x00000070))
                        .when(playing && !expanded, |bar| {
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
                        .when(playing && !expanded, |bar| {
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
                                        .on_click(
                                            cx.listener(|s, _, _, cx| {
                                                cx.stop_propagation();
                                                s.key("space", cx);
                                            }),
                                        ),
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
                                        icon("video-cancel", IconName::X, "Cancel download")
                                            .on_click(cx.listener(|s, _, _, cx| {
                                                cx.stop_propagation();
                                                s.cancel();
                                                cx.notify();
                                            })),
                                    ))
                                })
                                .child(
                                    icon(
                                        "video-browser",
                                        IconName::ExternalLink,
                                        "Open video in browser",
                                    )
                                    .on_click(
                                        move |_, _, cx| {
                                            cx.stop_propagation();
                                            cx.open_url(&url);
                                        },
                                    ),
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
                                    .on_click(
                                        move |_, window, cx| {
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
                                                            if old.entity_id()
                                                                != session.entity_id()
                                                            {
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
                                        },
                                    ),
                                )),
                        ),
                )
            })
            .into_any_element()
    }
}
