// Included in markdown_hosts, sharing the real-window fixtures.
#[test]
fn video_control_icons_are_embedded_in_the_application_assets() {
    use gpui::AssetSource as _;
    use gpui_kit_assets::IconName;
    let listed = crate::AppAssets.list("icons/").unwrap();
    for icon in [
        IconName::Play,
        IconName::Pause,
        IconName::Volume2,
        IconName::VolumeX,
        IconName::Maximize2,
        IconName::Minimize2,
        IconName::RotateCcw,
        IconName::ExternalLink,
        IconName::X,
    ] {
        let path = icon.path();
        let bytes = crate::AppAssets
            .load(&path)
            .unwrap()
            .unwrap_or_else(|| panic!("missing video control icon: {path}"));
        assert!(std::str::from_utf8(&bytes).unwrap().contains("<svg"));
        assert!(listed.contains(&path));
    }
}

fn pending_video_downloads(app: &gpui::Entity<ReviewApp>, cx: &mut gpui::TestAppContext) {
    // Real streamed ranges/redirects use socket tests; UI tests stay on GPUI's
    // deterministic scheduler and cannot be woken by foreign OS threads.
    cx.update(|cx| {
        let hub = app.read(cx).video_hub.clone();
        hub.update(cx, |hub, _| {
            hub.download = Arc::new(|_, _| Box::pin(futures::future::pending()))
        });
    });
}
fn video_conversation(
    cx: &mut gpui::TestAppContext,
    body: &str,
) -> (gpui::AnyWindowHandle, gpui::Entity<ReviewApp>) {
    let (window, app) = viewer_conversation(cx, body);
    pending_video_downloads(&app, cx);
    (window, app)
}
#[gpui::test]
fn extensionless_attachment_cards_use_metadata_once_and_leave_other_links_stock(
    cx: &mut gpui::TestAppContext,
) {
    image_client(cx);
    let body = "https://github.com/user-attachments/assets/video\n\nhttps://github.com/user-attachments/assets/image\n\n`https://github.com/user-attachments/assets/code`\n\n[ordinary](https://example.com/page)\n\nBefore [image](https://github.com/user-attachments/assets/image) after.";
    let (window, app) = video_conversation(cx, body);
    let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = probes.clone();
    cx.update(|cx| {
        app.read(cx).video_hub.clone().update(cx, |hub, _| {
            hub.probe = Arc::new(move |url| {
                counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Box::pin(async move { Ok(url.ends_with("/video")) })
            });
        })
    });
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.simulate_resize(size(px(1400.), px(1400.)));
    draw_settled(&mut vcx);
    assert_eq!(
        vcx.debug_bounds("conversation-0-video-0")
            .unwrap()
            .size
            .height,
        px(video::INLINE_HEIGHT)
    );
    let other = Box::leak(
        format!(
            "conversation-0-video-{}",
            body.find("https://github.com/user-attachments/assets/image")
                .unwrap()
        )
        .into_boxed_str(),
    );
    assert!(vcx.debug_bounds(other).unwrap().size.height < px(260.));
    let inline = Box::leak(
        format!("conversation-0-video-{}", body.find("[image]").unwrap()).into_boxed_str(),
    );
    let bounds = vcx.debug_bounds(inline).unwrap();
    assert!(
        bounds.size.height < px(60.) && bounds.size.width > px(1.),
        "non-video inline links do not retain a video-sized gap: {bounds:?}"
    );
    draw_settled(&mut vcx);
    assert_eq!(
        probes.load(std::sync::atomic::Ordering::Relaxed),
        2,
        "code/ordinary links are not probed; metadata is reused"
    );
    cx.update(|cx| assert!(app.read(cx).video_hub.read(cx).session.is_none()));
    click_selector(&mut vcx, "video-play");
    cx.update(|cx| {
        assert!(app
            .read(cx)
            .video_hub
            .read(cx)
            .session
            .as_ref()
            .unwrap()
            .read(cx)
            .target
            .url
            .ends_with("/video"))
    });
    assert!(cx.opened_url().is_none());
}

#[gpui::test]
fn video_cancel_retry_and_starting_another_card_release_the_old_session(
    cx: &mut gpui::TestAppContext,
) {
    image_client(cx);
    let (window, app) = video_conversation(
        cx,
        "[one](http://127.0.0.1:1/one.mp4)\n\n[two](http://127.0.0.1:1/two.mov)",
    );
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.simulate_resize(size(px(1400.), px(1400.)));
    draw_settled(&mut vcx);
    click_selector(&mut vcx, "video-play");
    let first = cx.update(|cx| app.read(cx).video_hub.read(cx).session.clone().unwrap());
    click_selector(&mut vcx, "video-cancel");
    assert!(vcx.debug_bounds("video-retry").is_some());
    click_selector(&mut vcx, "video-retry");
    cx.update(|cx| {
        assert_eq!(
            app.read(cx)
                .video_hub
                .read(cx)
                .session
                .as_ref()
                .unwrap()
                .entity_id(),
            first.entity_id()
        )
    });
    // The remaining Play video button is on the other, inactive card.
    click_selector(&mut vcx, "video-play");
    cx.update(|cx| {
        let hub = app.read(cx).video_hub.read(cx);
        assert_ne!(hub.session.as_ref().unwrap().entity_id(), first.entity_id());
        assert!(!first.read(cx).is_playing());
    });
    assert!(cx.opened_url().is_none());
}

#[gpui::test]
fn video_download_errors_are_retryable_and_never_open_a_browser(cx: &mut gpui::TestAppContext) {
    image_client(cx);
    let (window, app) = video_conversation(cx, "[Recording](http://127.0.0.1:1/recording.mp4)");
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = attempts.clone();
    cx.update(|cx| {
        app.read(cx).video_hub.clone().update(cx, |hub, _| {
            hub.download = Arc::new(move |_, _| {
                let attempt = counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Box::pin(async move {
                    if attempt == 0 {
                        Err(anyhow::anyhow!("Could not download video"))
                    } else {
                        futures::future::pending().await
                    }
                })
            });
        })
    });
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    draw_settled(&mut vcx);
    assert!(vcx.debug_bounds("video-error").is_some());
    assert!(vcx.debug_bounds("video-retry").is_some());
    assert!(cx.opened_url().is_none());
    click_selector(&mut vcx, "video-retry");
    assert!(vcx.debug_bounds("video-error").is_none());
    assert!(vcx.debug_bounds("video-loading").is_some());
    assert_eq!(attempts.load(std::sync::atomic::Ordering::Relaxed), 2);
    click_selector(&mut vcx, "video-cancel");
    assert!(vcx.debug_bounds("video-canceled").is_some());
}

#[gpui::test]
fn video_preloads_without_playing_and_picture_and_overlay_controls_toggle_once(
    cx: &mut gpui::TestAppContext,
) {
    image_client(cx);
    let (window, app) = video_conversation(cx, "[clip](http://127.0.0.1:1/clip.mp4)");
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = calls.clone();
    cx.update(|cx| {
        app.read(cx).video_hub.clone().update(cx, |hub, _| {
            hub.download = Arc::new(move |_, _| {
                counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Box::pin(futures::future::pending())
            });
        })
    });
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.simulate_resize(size(px(1400.), px(1000.)));
    draw_settled(&mut vcx);
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    cx.update(|cx| assert!(app.read(cx).video_hub.read(cx).session.is_none()));
    // A paused preview can expand without starting playback or a full fetch.
    click_selector(&mut vcx, "video-expand");
    assert!(vcx.debug_bounds("image-viewer").is_some());
    cx.update(|cx| {
        assert!(!app
            .read(cx)
            .video_hub
            .read(cx)
            .session
            .as_ref()
            .unwrap()
            .read(cx)
            .is_playing())
    });
    vcx.simulate_keystrokes("escape");
    draw_settled(&mut vcx);
    let picture = vcx.debug_bounds("video-session").unwrap();
    let chrome = vcx.debug_bounds("video-controls").unwrap();
    assert_eq!(picture.size.height, px(video::INLINE_HEIGHT));
    assert!(
        chrome.top() > picture.top() && chrome.bottom() <= picture.bottom(),
        "chrome overlays rather than shrinking the picture"
    );
    let point = point(picture.left() + px(20.), picture.top() + px(100.));
    vcx.simulate_click(point, gpui::Modifiers::default());
    draw_settled(&mut vcx);
    let session = cx.update(|cx| app.read(cx).video_hub.read(cx).session.clone().unwrap());
    cx.update(|cx| assert!(session.read(cx).is_playing()));
    vcx.simulate_click(point, gpui::Modifiers::default());
    draw_settled(&mut vcx);
    cx.update(|cx| assert!(!session.read(cx).is_playing()));
    click_selector(&mut vcx, "video-toggle");
    cx.update(|cx| {
        assert!(
            session.read(cx).is_playing(),
            "control clicks must not also toggle the picture"
        )
    });
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "Play reuses the preloaded source"
    );
    assert!(cx.opened_url().is_none());
}

#[gpui::test]
fn video_is_opt_in_and_overlay_moves_the_same_session(cx: &mut gpui::TestAppContext) {
    image_client(cx);
    let (window, app) = video_conversation(cx, "[Recording](http://127.0.0.1:1/recording.mp4)");
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.simulate_resize(size(px(1400.), px(900.)));
    draw_settled(&mut vcx);
    assert!(vcx.debug_bounds("video-card").is_some());
    cx.update(|cx| {
        assert!(
            app.read(cx).video_hub.read(cx).session.is_none(),
            "preloading never starts playback"
        )
    });
    click_selector(&mut vcx, "video-play");
    let session = cx.update(|cx| app.read(cx).video_hub.read(cx).session.clone().unwrap());
    vcx.update(|window, cx| {
        app.update(cx, |app, cx| {
            window.focus(&app.tree_filter_input.focus_handle(cx), cx)
        })
    });
    click_selector(&mut vcx, "video-expand");
    cx.update(|cx| {
        let app = app.read(cx);
        assert!(app.video_hub.read(cx).expanded);
        assert!(app.image_viewer.as_ref().unwrap().video.is_some());
        assert_eq!(
            app.video_hub.read(cx).session.as_ref().unwrap().entity_id(),
            session.entity_id()
        );
    });
    assert_eq!(
        vcx.debug_bounds("image-viewer").unwrap().size,
        size(px(1400.), px(900.) - APP_HEADER_HEIGHT)
    );
    assert!(
        vcx.debug_bounds("viewer-image").is_none(),
        "videos must not invoke the image loader"
    );
    assert!(vcx.debug_bounds("video-session").unwrap().size.height > px(260.));
    vcx.simulate_keystrokes("cmd-w cmd-o j tab");
    draw_settled(&mut vcx);
    cx.update(|cx| {
        let app = app.read(cx);
        assert_eq!(app.items.len(), 1);
        assert!(!app.quick_open_visible);
    });
    vcx.simulate_keystrokes("space");
    draw_settled(&mut vcx);
    cx.update(|cx| {
        assert!(
            !session.read(cx).is_playing(),
            "Space can pause a pending playback request"
        )
    });
    vcx.simulate_keystrokes("space");
    draw_settled(&mut vcx);
    cx.update(|cx| assert!(session.read(cx).is_playing()));
    vcx.simulate_keystrokes("escape");
    draw_settled(&mut vcx);
    vcx.update(|window, cx| {
        let app = app.read(cx);
        assert!(app.image_viewer.is_none());
        assert!(!app.video_hub.read(cx).expanded);
        assert_eq!(
            app.video_hub.read(cx).session.as_ref().unwrap().entity_id(),
            session.entity_id()
        );
        assert!(app.tree_filter_input.focus_handle(cx).is_focused(window));
    });
    assert!(cx.opened_url().is_none());
    cx.update(|cx| app.update(cx, |app, cx| app.close_item(app.active, cx)));
    draw_settled(&mut vcx);
    cx.update(|cx| assert!(app.read(cx).video_hub.read(cx).session.is_none()));
}

#[gpui::test]
fn video_pauses_on_pane_exit_and_cards_preserve_virtualized_geometry(
    cx: &mut gpui::TestAppContext,
) {
    image_client(cx);
    let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
    pending_video_downloads(&app, cx);
    let mut item = loop_pr_item(vec![rc(
        11,
        "a.rs",
        Some("RIGHT"),
        Some(1),
        "[clip](http://127.0.0.1:1/recording.mp4)",
        "alice",
        "2026-01-01T00:00:00Z",
        None,
    )]);
    let ItemState::Ready(data) = &mut item.state else {
        unreachable!()
    };
    data.diff = diff_core::parse_patch(
        "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n tail\n",
    );
    data.set_rows(data.row_build());
    install_item(&app, item, cx);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.simulate_resize(size(px(1500.), px(1000.)));
    for mode in [ViewMode::Unified, ViewMode::Split] {
        cx.update(|cx| {
            app.update(cx, |app, cx| {
                let data = app.active_data_mut().unwrap();
                data.mode = mode;
                data.rebuild_rows_anchored();
                data.scroll.0.borrow_mut().deferred_scroll_to_item = None;
                data.scroll
                    .0
                    .borrow()
                    .base_handle
                    .set_offset(point(px(0.), px(0.)));
                cx.notify();
            })
        });
        draw_settled(&mut vcx);
        assert_eq!(
            vcx.debug_bounds("inline-text-0-11-0-video-0")
                .unwrap()
                .size
                .height,
            px(video::INLINE_HEIGHT)
        );
        let before = cx.update(|cx| app.read(cx).active_data().unwrap().rows.len());
        click_selector(&mut vcx, "video-play");
        click_selector(&mut vcx, "video-expand");
        vcx.simulate_keystrokes("escape");
        draw_settled(&mut vcx);
        cx.update(|cx| assert_eq!(app.read(cx).active_data().unwrap().rows.len(), before));
        // Even if the top reservation is virtualized away the remaining card
        // remains a real element, not a replacement row or independent window.
        vcx.simulate_resize(size(px(1500.), px(600.)));
        cx.update(|cx| app.update(cx, |app, cx| {
            let data = app.active_data_mut().unwrap();
            let first = data.rows.iter().position(|row| matches!(row, Row::CommentBody { text, slot: 0, .. } if text.comment_id == 11)).unwrap();
            data.scroll.0.borrow().base_handle.set_offset(point(px(0.), -px((first + 1) as f32 * row_height()))); cx.notify();
        }));
        draw_settled(&mut vcx);
        assert!(vcx.debug_bounds("video-session").is_some());
        click_selector(&mut vcx, "video-expand");
        assert!(vcx.debug_bounds("image-viewer").is_some());
        vcx.simulate_keystrokes("escape");
        draw_settled(&mut vcx);
        cx.update(|cx| app.update(cx, |app, cx| app.flip_pane(ItemPane::Conversation, cx)));
        draw_settled(&mut vcx);
        cx.update(|cx| {
            assert!(!app
                .read(cx)
                .video_hub
                .read(cx)
                .session
                .as_ref()
                .unwrap()
                .read(cx)
                .is_playing())
        });
        cx.update(|cx| {
            app.update(cx, |app, cx| {
                app.video_hub.update(cx, |hub, cx| hub.release(cx));
                app.flip_pane(ItemPane::Changes, cx);
            })
        });
        vcx.simulate_resize(size(px(1500.), px(1000.)));
    }
}

#[gpui::test]
fn video_expanded_bounds_stay_below_header_and_inside_window(cx: &mut gpui::TestAppContext) {
    image_client(cx);
    let (window, _) = video_conversation(cx, "[clip](http://127.0.0.1:1/clip.mp4)");
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    draw_settled(&mut vcx);
    click_selector(&mut vcx, "video-expand");
    for viewport in [
        size(px(1400.), px(900.)),
        size(px(700.), px(500.)),
        size(px(1000.), px(700.)),
    ] {
        vcx.simulate_resize(viewport);
        draw_settled(&mut vcx);
        let overlay = vcx.debug_bounds("image-viewer").unwrap();
        let picture = vcx.debug_bounds("video-session").unwrap();
        let controls = vcx.debug_bounds("video-controls").unwrap();
        assert_eq!(overlay.top(), APP_HEADER_HEIGHT);
        assert_eq!(overlay.bottom(), viewport.height);
        assert!(
            picture.top() >= overlay.top() && picture.bottom() <= overlay.bottom(),
            "picture clipped: {picture:?} in {overlay:?}"
        );
        assert!(picture.left() >= overlay.left() && picture.right() <= overlay.right());
        assert!(picture.size.height >= overlay.size.height - px(40.));
        assert!(controls.top() >= picture.top() && controls.bottom() <= picture.bottom());
    }
}

#[gpui::test]
fn video_expanded_controls_hide_on_idle_and_window_exit(cx: &mut gpui::TestAppContext) {
    image_client(cx);
    let (window, app) = video_conversation(cx, "[clip](http://127.0.0.1:1/clip.mp4)");
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    draw_settled(&mut vcx);
    click_selector(&mut vcx, "video-expand");
    let picture = vcx.debug_bounds("video-session").unwrap();
    let p = picture.center();
    vcx.simulate_mouse_move(p, None, Default::default());
    draw_settled(&mut vcx);
    vcx.executor()
        .advance_clock(std::time::Duration::from_millis(1500));
    draw_settled(&mut vcx);
    assert!(vcx.debug_bounds("video-controls").is_some());
    vcx.simulate_mouse_move(p + point(px(5.), px(0.)), None, Default::default());
    draw_settled(&mut vcx);
    vcx.executor()
        .advance_clock(std::time::Duration::from_millis(1500));
    draw_settled(&mut vcx);
    assert!(
        vcx.debug_bounds("video-controls").is_some(),
        "movement resets the deadline"
    );
    vcx.executor()
        .advance_clock(std::time::Duration::from_millis(600));
    draw_settled(&mut vcx);
    assert!(vcx.debug_bounds("video-controls").is_none());
    assert!(
        vcx.debug_bounds("video-play").is_none(),
        "paused center Play also hides"
    );
    // A synthetic stationary move must not defeat the idle deadline.
    vcx.simulate_mouse_move(p + point(px(5.), px(0.)), None, Default::default());
    draw_settled(&mut vcx);
    assert!(vcx.debug_bounds("video-controls").is_none());
    vcx.simulate_mouse_move(p, None, Default::default());
    draw_settled(&mut vcx);
    assert!(vcx.debug_bounds("video-controls").is_some());
    vcx.simulate_event(gpui::MouseExitEvent {
        position: p,
        ..Default::default()
    });
    draw_settled(&mut vcx);
    assert!(
        vcx.debug_bounds("video-controls").is_none(),
        "leaving the window hides immediately"
    );
    vcx.simulate_mouse_move(p, None, Default::default());
    draw_settled(&mut vcx);
    assert!(
        vcx.debug_bounds("video-controls").is_some(),
        "reentering at the same position reveals controls"
    );
    let session = cx.update(|cx| app.read(cx).video_hub.read(cx).session.clone().unwrap());
    vcx.simulate_keystrokes("space");
    draw_settled(&mut vcx);
    vcx.executor()
        .advance_clock(std::time::Duration::from_millis(2100));
    draw_settled(&mut vcx);
    assert!(
        vcx.debug_bounds("video-controls").is_none(),
        "playing chrome also idles"
    );
    cx.update(|cx| assert!(session.read(cx).is_playing()));
    vcx.simulate_keystrokes("escape");
    draw_settled(&mut vcx);
    assert!(
        vcx.debug_bounds("video-controls").is_some(),
        "returning inline resets expanded hiding"
    );
}

#[gpui::test]
fn video_sliders_drag_clamp_release_and_never_toggle_playback(cx: &mut gpui::TestAppContext) {
    image_client(cx);
    let (window, app) = video_conversation(cx, "[clip](http://127.0.0.1:1/clip.mp4)");
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    draw_settled(&mut vcx);
    click_selector(&mut vcx, "video-expand");
    let session = cx.update(|cx| app.read(cx).video_hub.read(cx).session.clone().unwrap());
    for expanded in [true, false] {
        if !expanded {
            vcx.simulate_keystrokes("escape");
            draw_settled(&mut vcx);
        }
        for (selector, kind) in [("video-seek", "seek"), ("video-volume", "volume")] {
            let b = vcx.debug_bounds(selector).unwrap();
            let p = point(b.left() + b.size.width * 0.25, b.center().y);
            vcx.simulate_mouse_down(p, MouseButton::Left, Default::default());
            draw_settled(&mut vcx);
            cx.update(|cx| {
                let (actual, value) = session.read(cx).slider_gesture().unwrap();
                assert_eq!(actual, kind);
                assert!((value - 0.25).abs() < 0.01);
            });
            // Cross the slider vertically: capture must continue the same gesture.
            let p = point(b.left() + b.size.width * 0.75, b.top() - px(30.));
            vcx.simulate_mouse_move(p, MouseButton::Left, Default::default());
            draw_settled(&mut vcx);
            cx.update(|cx| {
                assert!((session.read(cx).slider_gesture().unwrap().1 - 0.75).abs() < 0.01)
            });
            if expanded {
                vcx.executor()
                    .advance_clock(std::time::Duration::from_millis(2100));
                draw_settled(&mut vcx);
                assert!(
                    vcx.debug_bounds("video-controls").is_some(),
                    "do not hide during a gesture"
                );
            }
            for (x, value) in [(b.right() + px(20.), 1.), (b.left() - px(20.), 0.)] {
                let p = point(x, b.top() - px(30.));
                vcx.simulate_mouse_move(p, MouseButton::Left, Default::default());
                draw_settled(&mut vcx);
                cx.update(|cx| assert_eq!(session.read(cx).slider_gesture().unwrap().1, value));
            }
            let p = if expanded {
                point(px(1.), APP_HEADER_HEIGHT + px(1.)) // outside the video, on its backdrop
            } else {
                point(b.center().x, b.top() - px(30.))
            };
            vcx.simulate_mouse_up(p, MouseButton::Left, Default::default());
            draw_settled(&mut vcx);
            cx.update(|cx| {
                assert!(session.read(cx).slider_gesture().is_none());
                assert!(
                    !session.read(cx).is_playing(),
                    "slider release must not click the picture"
                );
            });
            assert_eq!(
                vcx.debug_bounds("image-viewer").is_some(),
                expanded,
                "slider release must not dismiss the viewer"
            );
            vcx.simulate_mouse_move(p, None, Default::default());
            draw_settled(&mut vcx);
            cx.update(|cx| assert!(session.read(cx).slider_gesture().is_none()));
        }
    }
    let picture = vcx.debug_bounds("video-session").unwrap();
    vcx.simulate_click(
        picture.origin + point(px(20.), px(100.)),
        Default::default(),
    );
    draw_settled(&mut vcx);
    cx.update(|cx| {
        assert!(
            session.read(cx).is_playing(),
            "a subsequent picture click still works"
        )
    });
}

#[gpui::test]
fn video_html_and_mixed_link_cards_use_shared_markdown_hosts(cx: &mut gpui::TestAppContext) {
    image_client(cx);
    let (window, app) = video_conversation(cx,
        "<video autoplay controls src='http://127.0.0.1:1/a.mov'></video>\n\nBefore [clip](http://127.0.0.1:1/b.webm) after.\n\n`http://127.0.0.1:1/code.mp4`");
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.simulate_resize(size(px(1400.), px(1500.)));
    draw_settled(&mut vcx);
    assert!(vcx.debug_bounds("conversation-0-video-0").is_some());
    cx.update(|cx| assert!(app.read(cx).video_hub.read(cx).session.is_none()));
    // Conversation comments/review threads use exactly the same host.
    cx.update(|cx| {
        app.update(cx, |app, cx| {
            let data = app.active_data_mut().unwrap();
            let mut discussion = conversation::tests::sample_discussion();
            discussion.comments.truncate(1);
            discussion.comments[0].body = "[video](http://127.0.0.1:1/comment.mp4)".into();
            discussion.threads[0].is_resolved = false;
            discussion.threads[0].is_outdated = false;
            discussion.threads[0].comments.truncate(1);
            discussion.threads[0].comments[0].body =
                "<video src='http://127.0.0.1:1/thread.mov'></video>".into();
            data.discussion = Some(discussion);
            cx.notify();
        })
    });
    vcx.simulate_resize(size(px(1400.), px(3000.)));
    draw_settled(&mut vcx);
    assert!(vcx
        .debug_bounds("discussion-0-main-false-1-video-0")
        .is_some());
    assert!(vcx
        .debug_bounds("discussion-0-review-false-1-video-0")
        .is_some());
}
