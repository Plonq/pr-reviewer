use super::*;

fn launch_session_window(
    cx: &mut gpui::TestAppContext,
    store: local_store::LocalStore,
    sources: Vec<Source>,
    errors: Vec<String>,
    shift: bool,
) -> (gpui::AnyWindowHandle, gpui::Entity<ReviewApp>) {
    cx.update(|cx| {
        gpui::component::init(cx);
        theme::apply_ui_theme(cx);
    });
    let stash = Rc::new(RefCell::new(None));
    let stash2 = stash.clone();
    let window = cx.add_window(move |window, cx| {
        window.set_modifiers(gpui::Modifiers {
            shift,
            ..Default::default()
        });
        let app = cx.new(|cx| {
            ReviewApp::new(
                sources,
                errors,
                Arc::new(FakePr::default()),
                store,
                None,
                window,
                cx,
            )
        });
        *stash2.borrow_mut() = Some(app.clone());
        Root::new(app, window, cx)
    });
    let app = stash.borrow().clone().unwrap();
    cx.run_until_parked();
    (window.into(), app)
}

fn close_session_window(
    window: gpui::AnyWindowHandle,
    app: gpui::Entity<ReviewApp>,
    cx: &mut gpui::TestAppContext,
) {
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.update(|window, _| window.remove_window());
    drop(vcx);
    drop(app);
    cx.update(|_| {});
    cx.run_until_parked();
}

fn seed_saved_session(cx: &mut gpui::TestAppContext) -> local_store::LocalStore {
    let store = local_store::LocalStore::in_memory();
    let fake = Arc::new(FakePr::default());
    let (window, app) = queue_window_with_store(cx, fake.clone(), store.clone());
    install_item(&app, commenting_item(&fake), cx);
    close_session_window(window, app, cx);
    assert_eq!(
        store.setting(session::KEY).unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    store
}

#[gpui::test]
fn session_restores_reviews_editors_and_view_on_every_launch(cx: &mut gpui::TestAppContext) {
    let fake = Arc::new(FakePr::default());
    let store = local_store::LocalStore::in_memory();
    let (window, app) = queue_window_with_store(cx, fake.clone(), store.clone());
    install_item(&app, commenting_item(&fake), cx);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.update(|window, cx| {
        app.update(cx, |app, cx| {
            app.items[0].pane = ItemPane::Changes;
            app.show_queue = false;
            app.minimap_visible = false;
            app.open_composer(None, "a.rs".into(), CommentSide::Right, 1, 0, window, cx);
            app.composer().unwrap().input.update(cx, |input, cx| {
                input.set_value("unfinished inline comment", window, cx);
            });
            app.open_review(window, cx);
            app.review.as_ref().unwrap().input.update(cx, |input, cx| {
                input.set_value("unfinished summary", window, cx);
            });
            app.save_session(cx).unwrap();
            assert_eq!(
                store.setting(session::KEY).unwrap()["items"][0]["summary_commit_id"],
                "dead10cc"
            );
            app.close_review(window, cx);
            app.save_session(cx).unwrap();
            assert!(session::read(&store).is_some());
        });
    });
    *fake.head.lock().unwrap() = Some("new-head-B".into());
    let (_, restored) = queue_window_with_store(cx, fake.clone(), store.clone());
    cx.update(|cx| {
        let app = restored.read(cx);
        assert_eq!(app.items.len(), 1);
        assert_eq!(app.active, 0);
        assert!(!app.show_queue);
        assert!(!app.minimap_visible);
        assert_eq!(app.items[0].pane, ItemPane::Changes);
        assert_eq!(app.composers.len(), 1);
        let composer = &app.composers[0];
        assert_eq!(
            composer.input.read(cx).value().as_str(),
            "unfinished inline comment"
        );
        assert_eq!(composer.path, "a.rs");
        assert_eq!(composer.line, 1);
        assert!(!composer.in_flight);
        assert_eq!(
            app.items[0].review_draft.as_ref().unwrap().commit_id,
            "dead10cc"
        );
        assert_eq!(
            app.items[0]
                .review_draft
                .as_ref()
                .unwrap()
                .input
                .read(cx)
                .value()
                .as_str(),
            "unfinished summary"
        );
        assert!(session::read(&store).is_some());
    });
    let (_, again) = queue_window_with_store(cx, fake, store);
    cx.update(|cx| {
        assert_eq!(again.read(cx).items.len(), 1);
        assert_eq!(again.read(cx).composers.len(), 1);
    });
}

#[gpui::test]
fn legacy_summary_requires_clearing_and_reopening_to_capture_a_reviewed_commit(
    cx: &mut gpui::TestAppContext,
) {
    let store = seed_saved_session(cx);
    let mut saved = store.setting(session::KEY).unwrap();
    saved["items"][0]["summary"] = serde_json::json!("legacy summary");
    saved["items"][0]
        .as_object_mut()
        .unwrap()
        .remove("summary_commit_id");
    store.set_setting(session::KEY, saved);
    let fake = Arc::new(FakePr::default());
    let (window, app) = queue_window_with_store(cx, fake.clone(), store);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.update(|window, cx| {
        app.update(cx, |app, cx| {
            app.open_review(window, cx);
            app.submit_review(window, cx);
        })
    });
    vcx.run_until_parked();
    vcx.update(|_, cx| {
        let review = app.read(cx).review.as_ref().unwrap();
        assert!(review
            .error
            .as_ref()
            .unwrap()
            .contains("missing reviewed head commit"));
        assert_eq!(review.input.read(cx).value().as_str(), "legacy summary");
    });
    assert!(fake.posts().is_empty());
    vcx.update(|window, cx| {
        app.update(cx, |app, cx| {
            app.review
                .as_ref()
                .unwrap()
                .input
                .update(cx, |input, cx| input.set_value("  \n", window, cx));
            app.close_review(window, cx);
            assert!(app.items[0].review_draft.is_none());
            app.open_review(window, cx);
            let review = app.review.as_ref().unwrap();
            assert_eq!(review.commit_id, "dead10cc");
            assert!(review.input.read(cx).value().is_empty());
            app.submit_review(window, cx);
        })
    });
    vcx.run_until_parked();
    assert_eq!(*fake.review_commits.lock().unwrap(), vec!["dead10cc"]);
    assert_eq!(
        fake.posts(),
        vec![Post::Review {
            verdict: github::ReviewVerdict::Approve,
            body: String::new(),
        }]
    );
}

#[gpui::test]
fn update_restart_is_blocked_by_writes_and_store_failure(cx: &mut gpui::TestAppContext) {
    let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.update(|_, cx| {
        app.update(cx, |app, cx| {
            app.updater.state = 1;
            app.sessions.insert(
                42,
                SessionUi {
                    busy: true,
                    ..Default::default()
                },
            );
            assert!(app.update_blocked(cx));
            app.install_update(cx);
            assert_eq!(app.updater.state, 1);
            assert!(session::read(&app.store).is_none());
            app.sessions.clear();
            app.store_error = Some("disk unavailable".into());
            app.install_update(cx);
            assert_eq!(app.updater.state, 1);
            assert!(session::read(&app.store).is_none());
            app.updater.state = 0;
        })
    });
}

#[test]
fn malformed_session_snapshot_does_not_crash_launch() {
    let store = local_store::LocalStore::in_memory();
    store.set_setting(session::KEY, serde_json::json!({"items": "broken"}));
    assert!(session::read(&store).is_none());
}

#[gpui::test]
fn update_button_is_quiet_until_ready_and_restarts_through_click(cx: &mut gpui::TestAppContext) {
    let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    draw_window(&mut vcx);
    assert!(vcx.debug_bounds("update-button").is_none());
    vcx.update(|_, cx| {
        app.update(cx, |app, cx| {
            app.updater.state = 1;
            cx.notify();
        })
    });
    draw_window(&mut vcx);
    let button = vcx
        .debug_bounds("update-button")
        .expect("quiet header button");
    assert!(button.size.height <= APP_HEADER_HEIGHT);
    vcx.simulate_click(button.center(), gpui::Modifiers::default());
    vcx.update(|_, cx| {
        assert_eq!(app.read(cx).updater.state, 2);
        assert!(session::read(&app.read(cx).store).is_some());
    });
}

#[gpui::test]
fn session_keeps_incremental_baseline_and_position(cx: &mut gpui::TestAppContext) {
    let fake = since_review_fake();
    let store = local_store::LocalStore::in_memory();
    let (window, app) = queue_window_with_store(cx, fake.clone(), store.clone());
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.update(|_, cx| app.update(cx, |app, cx| app.open_item(Source::Pr(loop_loc()), cx)));
    vcx.run_until_parked();
    vcx.update(|window, cx| app.update(cx, |app, cx| app.switch_review_scope(true, window, cx)));
    vcx.run_until_parked();
    let prior = vcx.update(|_, cx| {
        app.update(cx, |app, cx| {
            let data = app.active_data().unwrap();
            assert!(data.review_base.is_some());
            let prior = (data.review_base.clone(), data.review_position());
            app.save_session(cx).unwrap();
            prior
        })
    });
    let (_, restored) = queue_window_with_store(cx, fake, store);
    cx.update(|cx| {
        let data = restored.read(cx).active_data().unwrap();
        assert_eq!(data.review_base, prior.0);
        assert_eq!(data.review_position(), prior.1);
    });
}

#[gpui::test]
fn normal_window_close_saves_session_without_an_update(cx: &mut gpui::TestAppContext) {
    let store = local_store::LocalStore::in_memory();
    let (window, app) = queue_window_with_store(cx, Arc::new(FakePr::default()), store.clone());
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.update(|window, cx| {
        app.update(cx, |app, _| {
            assert_eq!(app.updater.state, 0);
            app.minimap_visible = false;
        });
        window.remove_window();
    });
    drop(vcx);
    drop(app);
    cx.update(|_| {});
    cx.run_until_parked();
    assert!(session::read(&store).is_some());
    let (_, app) = queue_window_with_store(cx, Arc::new(FakePr::default()), store);
    cx.update(|cx| assert!(!app.read(cx).minimap_visible));
}

#[gpui::test]
fn shift_launch_skips_restore_once_and_preserves_snapshot_until_quit(
    cx: &mut gpui::TestAppContext,
) {
    let store = seed_saved_session(cx);
    let saved = store.setting(session::KEY).unwrap();
    let (window, fresh) = launch_session_window(cx, store.clone(), vec![], vec![], true);
    cx.update(|cx| {
        assert!(fresh.read(cx).items.is_empty());
        assert!(fresh.read(cx).show_queue);
    });
    cx.executor()
        .advance_clock(session::CHECKPOINT_INTERVAL * 2);
    cx.run_until_parked();
    assert_eq!(store.setting(session::KEY), Some(saved));

    // Reading/skipping a snapshot doesn't consume it or set an opt-out preference.
    let (again_window, again) = launch_session_window(cx, store.clone(), vec![], vec![], false);
    cx.update(|cx| assert_eq!(again.read(cx).items.len(), 1));
    close_session_window(again_window, again, cx);
    close_session_window(window, fresh, cx);

    let (_, next) = launch_session_window(cx, store, vec![], vec![], false);
    cx.update(|cx| {
        assert!(
            next.read(cx).items.is_empty(),
            "quitting the fresh session replaces the old one"
        );
        assert!(next.read(cx).show_queue);
    });
}

#[gpui::test]
fn cli_targets_replace_restore_but_do_not_clear_it_until_quit(cx: &mut gpui::TestAppContext) {
    let store = seed_saved_session(cx);
    let saved = store.setting(session::KEY).unwrap();
    let explicit = github::PrLocator {
        number: 99,
        ..loop_loc()
    };
    let (window, app) = launch_session_window(
        cx,
        store.clone(),
        vec![Source::Pr(explicit.clone())],
        vec![],
        false,
    );
    cx.update(|cx| {
        let app = app.read(cx);
        assert_eq!(app.items.len(), 1);
        assert!(sources_match(
            &app.items[0].source,
            &Source::Pr(explicit.clone())
        ));
    });
    cx.executor()
        .advance_clock(session::CHECKPOINT_INTERVAL * 2);
    cx.run_until_parked();
    assert_eq!(store.setting(session::KEY), Some(saved));
    close_session_window(window, app, cx);

    let (_, restored) = launch_session_window(cx, store, vec![], vec![], false);
    cx.update(|cx| {
        assert!(sources_match(
            &restored.read(cx).items[0].source,
            &Source::Pr(explicit)
        ))
    });
}

#[gpui::test]
fn invalid_cli_arguments_do_not_restore_over_the_startup_error(cx: &mut gpui::TestAppContext) {
    let store = seed_saved_session(cx);
    let saved = store.setting(session::KEY).unwrap();
    let (_, app) = launch_session_window(
        cx,
        store.clone(),
        vec![],
        vec!["invalid PR argument".into()],
        false,
    );
    cx.update(|cx| {
        assert!(app.read(cx).items.is_empty());
        assert!(app.read(cx).quick_open_visible);
        assert_eq!(
            app.read(cx).open_error.as_deref(),
            Some("invalid PR argument")
        );
    });
    assert_eq!(store.setting(session::KEY), Some(saved));
}

#[gpui::test]
fn session_checkpoints_recover_without_a_clean_quit(cx: &mut gpui::TestAppContext) {
    let fake = Arc::new(FakePr::default());
    let store = local_store::LocalStore::in_memory();
    let (window, app) = queue_window_with_store(cx, fake.clone(), store.clone());
    install_item(&app, commenting_item(&fake), cx);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.update(|window, cx| {
        app.update(cx, |app, cx| {
            app.open_composer(None, "a.rs".into(), CommentSide::Right, 1, 0, window, cx);
            app.composer().unwrap().input.update(cx, |input, cx| {
                input.set_value("checkpoint draft", window, cx)
            });
            app.minimap_visible = false;
        })
    });
    assert!(store.setting(session::KEY).is_none());
    vcx.executor().advance_clock(session::CHECKPOINT_INTERVAL);
    vcx.run_until_parked();
    assert!(session::read(&store).is_some());

    // No quit or release has occurred: launch directly from the checkpoint.
    let (_, restored) = queue_window_with_store(cx, fake, store.clone());
    cx.update(|cx| {
        let app = restored.read(cx);
        assert!(!app.minimap_visible);
        assert_eq!(
            app.composers[0].input.read(cx).value().as_str(),
            "checkpoint draft"
        );
    });

    // Checkpoints skip unchanged state rather than rewriting SQLite every tick.
    let checkpoint = store.setting(session::KEY).unwrap();
    store.set_setting(session::KEY, serde_json::Value::Null);
    vcx.update(|_, cx| app.update(cx, |app, cx| app.checkpoint_session(cx).unwrap()));
    assert_eq!(store.setting(session::KEY), Some(serde_json::Value::Null));
    store.set_setting(session::KEY, checkpoint);

    vcx.update(|_, cx| app.update(cx, |app, _| app.minimap_visible = true));
    drop(vcx);
    close_session_window(window, app, cx);
    assert_eq!(
        store.setting(session::KEY).unwrap()["minimap"],
        true,
        "quit flushes changes made since the last checkpoint"
    );
}

#[gpui::test]
fn update_only_snapshots_migrate_to_general_sessions(cx: &mut gpui::TestAppContext) {
    let original = seed_saved_session(cx);
    let store = local_store::LocalStore::in_memory();
    store.set_setting("update_restart_v1", original.setting(session::KEY).unwrap());
    let (window, app) = launch_session_window(cx, store.clone(), vec![], vec![], false);
    cx.update(|cx| assert_eq!(app.read(cx).items.len(), 1));
    assert!(
        store.setting(session::KEY).is_none(),
        "restoration doesn't consume or rewrite the old snapshot"
    );
    close_session_window(window, app, cx);
    assert!(store.setting(session::KEY).is_some());

    store.set_setting("update_restart_v1", serde_json::Value::Null);
    let (_, app) = launch_session_window(cx, store, vec![], vec![], false);
    cx.update(|cx| assert_eq!(app.read(cx).items.len(), 1));
}
