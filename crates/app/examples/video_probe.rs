//! macOS smoke check: cargo run -p gander --example video_probe -- /absolute/file.mp4
//! Space: play/pause; arrows: seek; m: mute; Esc: quit. No network access.
#[path = "../src/video_player.rs"]
mod video_player;
use gpui::{prelude::*, *};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

struct Probe {
    player: video_player::VideoPlayer,
    start: Instant,
    reported: bool,
    focus: FocusHandle,
    verify: bool,
    stage: u8,
    paused_at: f64,
}
impl Probe {
    fn poll(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = self.player.tick() {
            eprintln!("{error:#}");
            cx.quit();
            return;
        }
        if self.player.frame.is_some() && !self.reported {
            let frame = self.player.frame.as_ref().unwrap();
            eprintln!(
                "Decoded NV12 frame {}x{}, duration {:.2}s",
                objc2_core_video::CVPixelBufferGetWidth(frame),
                objc2_core_video::CVPixelBufferGetHeight(frame),
                self.player.duration()
            );
            self.reported = true;
        }
        if self.verify && self.reported {
            let elapsed = self.start.elapsed().as_secs_f32();
            if elapsed > 1. && self.stage == 0 {
                self.player.pause();
                self.paused_at = self.player.position();
                self.stage = 1;
            } else if elapsed > 2. && self.stage == 1 {
                assert!(
                    (self.player.position() - self.paused_at).abs() < 0.1,
                    "pause drift"
                );
                self.player.seek(3.);
                self.stage = 2;
            } else if elapsed > 3. && self.stage == 2 {
                assert!((self.player.position() - 3.).abs() < 0.2, "seek failed");
                self.player.set_muted(true);
                assert!(self.player.muted());
                self.player.set_muted(false);
                assert!(!self.player.muted());
                self.player.set_volume(0.5);
                assert!((self.player.volume() - 0.5).abs() < 0.01);
                self.player.play();
                self.stage = 3;
                eprintln!("Verified pause, seek, mute/unmute and volume; resumed");
            } else if elapsed > 4. && self.stage == 3 {
                assert!(self.player.position() > 3.5, "resume failed");
                self.stage = 4;
            }
        }
        if self.start.elapsed() > Duration::from_secs(15) {
            eprintln!(
                "position {:.2}, frame {}",
                self.player.position(),
                self.reported
            );
            cx.quit();
        }
        cx.notify();
    }
}
impl Render for Probe {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("video-probe")
            .track_focus(&self.focus)
            .size_full()
            .bg(rgb(0x101114))
            .text_color(rgb(0xffffff))
            .on_key_down(cx.listener(|probe, event: &KeyDownEvent, _, cx| {
                match event.keystroke.key.as_str() {
                    "space" => {
                        if probe.player.playing() {
                            probe.player.pause()
                        } else {
                            probe.player.play()
                        }
                    }
                    "right" => probe.player.seek(probe.player.position() + 1.),
                    "left" => probe.player.seek(probe.player.position() - 1.),
                    "m" => probe.player.set_muted(!probe.player.muted()),
                    "escape" => cx.quit(),
                    _ => {}
                }
            }))
            .flex()
            .flex_col()
            .child(format!(
                "{:.2} / {:.2} seconds — Space, ←/→, M",
                self.player.position(),
                self.player.duration()
            ))
            .child(
                div().flex_1().w_full().overflow_hidden().children(
                    self.player
                        .frame
                        .clone()
                        .map(|frame| surface(frame).size_full()),
                ),
            )
    }
}
fn main() {
    let file = PathBuf::from(std::env::args().nth(1).expect("local video path"));
    eprintln!("Starting probe for {}", file.display());
    gpui::application().run(move |cx| {
        eprintln!("Opening probe window");
        cx.open_window(WindowOptions::default(), |window, cx| {
            let mut player = video_player::VideoPlayer::new(&file).unwrap();
            eprintln!("Player constructed");
            // Explicit playback in this diagnostic only; the application never autoplays.
            player.play();
            let focus = cx.focus_handle();
            window.focus(&focus, cx);
            cx.new(|cx| {
                cx.spawn(async move |this, cx| loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(16))
                        .await;
                    if this
                        .update(cx, |probe: &mut Probe, cx| probe.poll(cx))
                        .is_err()
                    {
                        break;
                    }
                })
                .detach();
                Probe {
                    player,
                    start: Instant::now(),
                    reported: false,
                    focus,
                    verify: std::env::args().any(|arg| arg == "--verify"),
                    stage: 0,
                    paused_at: 0.,
                }
            })
        })
        .unwrap();
        cx.activate(true);
    });
}
