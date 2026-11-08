use super::*;
use github::{MutationReceipt, MutationTarget};

fn discussion() -> PrDiscussion {
    let mut d = PrDiscussion::from_review_comments(
        (1..=3)
            .map(|id| github::ReviewComment {
                id,
                path: "a.rs".into(),
                side: Some("RIGHT".into()),
                line: Some(7),
                start_line: Some(5),
                body: format!("body {id}"),
                user: github::Author {
                    login: "alice".into(),
                },
                created_at: format!("2026-01-01T00:00:0{id}Z"),
                in_reply_to_id: (id != 1).then_some(1),
            })
            .collect(),
    );
    for c in &mut d.threads[0].comments {
        c.permissions.node_id = format!("node-{}", c.id);
    }
    d
}
fn draft(id: u64, parent: Option<u64>) -> PendingComment {
    PendingComment {
        id,
        node_id: format!("node-{id}"),
        path: "a.rs".into(),
        side: github::DiffSide::Right,
        line: Some(7),
        start_line: Some(5),
        body: format!("draft {id}"),
        in_reply_to_id: parent,
    }
}
fn review(comments: Vec<PendingComment>) -> PendingReview {
    PendingReview {
        id: 90,
        node_id: "review".into(),
        body: String::new(),
        comments,
    }
}
fn change(token: u64, key: CommentKey, body: Option<&str>) -> mutations::Change {
    let target = match key {
        CommentKey::Inline(id) => {
            MutationTarget::Published(github::CommentTarget::ReviewComment(format!("node-{id}")))
        }
        CommentKey::Pending(id) => MutationTarget::Pending {
            review_id: 90,
            id,
            node_id: format!("node-{id}"),
        },
        _ => unreachable!(),
    };
    mutations::Change {
        token,
        key,
        target,
        body: body.map(str::to_owned),
        recovery_body: body.unwrap_or("original").into(),
        placement: crate::ComposerPlacement::Diff,
        uncertain: false,
        error: None,
    }
}
fn receipt(c: &mutations::Change) -> MutationReceipt {
    match &c.body {
        Some(body) => MutationReceipt::Edited {
            target: c.target.clone(),
            body: body.clone(),
        },
        None => MutationReceipt::Deleted {
            target: c.target.clone(),
        },
    }
}
fn bodies(index: &index::CommentIndex) -> Vec<&str> {
    index.threads["a.rs"]
        .values()
        .flatten()
        .flat_map(|t| std::iter::once(&t.root).chain(&t.replies))
        .map(|c| c.body.as_str())
        .collect()
}

#[test]
fn deleting_a_duplicated_pending_root_never_turns_its_replies_into_public_cards() {
    let d = discussion();
    let mut p = review(vec![draft(1, None), draft(2, Some(1)), draft(3, Some(1))]);
    let mut posts = posting::Posts::default();
    let op = change(1, CommentKey::Pending(1), None);
    posts.changes.begin(op.clone(), Some(&d), Some(&p));
    for confirmed in [false, true] {
        if confirmed {
            p.comments.remove(0);
            posts.changes.finish(1, &Ok(receipt(&op)));
        }
        let view = Projection::new(Some(&d), Some(&p), &posts);
        assert!(view.conversation().is_empty());
        assert_eq!(view.pending_count(), 2);
        assert_eq!(view.diff_index().counts["a.rs"], (2, 0));
    }
}

#[test]
fn edits_replace_bodies_across_views_without_mutating_confirmed_inputs() {
    let d = discussion();
    let p = review(vec![draft(10, Some(1))]);
    let mut posts = posting::Posts::default();
    posts.changes.begin(
        change(1, CommentKey::Inline(2), Some("published edit")),
        Some(&d),
        Some(&p),
    );
    posts.changes.begin(
        change(2, CommentKey::Pending(10), Some("pending edit")),
        Some(&d),
        Some(&p),
    );
    let view = Projection::new(Some(&d), Some(&p), &posts);
    assert_eq!(view.body(view.inline(2).unwrap(), true), "published edit");
    assert_eq!(view.body(view.inline(10).unwrap(), true), "pending edit");
    assert_eq!(
        bodies(&view.diff_index()),
        ["body 1", "published edit", "body 3", "pending edit"]
    );
    assert_eq!(view.pending_count(), 1);
    let Entry::Thread(t) = &view.conversation()[0] else {
        panic!()
    };
    assert_eq!(view.body(t.comments[1], true), "published edit");
    assert_eq!(view.body(t.comments[3], true), "pending edit");
    assert_eq!(d.threads[0].comments[1].body, "body 2");
    assert_eq!(p.comments[0].body, "draft 10");
}

#[test]
fn published_root_deletion_retains_replies_thread_identity_and_historical_anchor() {
    let mut d = discussion();
    d.threads[0].line = None;
    d.threads[0].is_outdated = true;
    d.threads[0].is_resolved = true;
    d.threads[0].original_line = Some(7);
    d.threads[0].original_start_line = Some(5);
    let p = review(vec![draft(10, Some(1))]);
    let mut posts = posting::Posts::default();
    let op = change(1, CommentKey::Inline(1), None);
    posts.changes.begin(op.clone(), Some(&d), Some(&p));
    for confirmed in [false, true] {
        if confirmed {
            d.threads[0].comments.remove(0);
            posts.changes.finish(1, &Ok(receipt(&op)));
        }
        let view = Projection::new(Some(&d), Some(&p), &posts);
        let Entry::Thread(t) = &view.conversation()[0] else {
            panic!()
        };
        assert_eq!(t.key, ThreadKey("1".into()));
        assert_eq!(t.comments.len(), 3);
        assert_eq!(t.confirmed.original_line, Some(7));
        assert!(t.confirmed.is_resolved && t.confirmed.is_outdated);
        assert_eq!(view.reply_target(t.confirmed), Some(2));
        assert_eq!(view.diff_index().counts["a.rs"], (0, 3));
    }
    // Last published members disappear; pending replies still own the original
    // thread shell and can reply to a surviving identity, never the deleted root.
    for id in [2, 3] {
        let op = change(id, CommentKey::Inline(id), None);
        posts.changes.begin(op.clone(), Some(&d), Some(&p));
        d.threads[0].comments.retain(|c| c.id != id);
        posts.changes.finish(id, &Ok(receipt(&op)));
    }
    d.threads.clear();
    let view = Projection::new(Some(&d), Some(&p), &posts);
    let Entry::Thread(t) = &view.conversation()[0] else {
        panic!()
    };
    assert_eq!(t.key, ThreadKey("1".into()));
    assert_eq!(t.comments.len(), 1);
    assert_eq!(view.reply_target(t.confirmed), Some(10));
    assert_eq!(view.diff_index().counts["a.rs"], (0, 1));
}

#[test]
fn pending_root_deletion_promotes_survivors_even_after_a_flat_snapshot_loses_the_root() {
    let mut p = review(vec![
        draft(10, None),
        draft(11, Some(10)),
        draft(12, Some(11)),
    ]);
    p.comments[1].line = None;
    p.comments[2].line = Some(99);
    let mut posts = posting::Posts::default();
    let op = change(1, CommentKey::Pending(10), None);
    posts.changes.begin(op.clone(), None, Some(&p));
    for confirmed in [false, true] {
        if confirmed {
            p.comments.remove(0);
            posts.changes.finish(1, &Ok(receipt(&op)));
            posts.changes.reconcile(None, Some(&p), false);
        }
        let view = Projection::new(None, Some(&p), &posts);
        assert_eq!(view.pending_count(), 2);
        let index = view.diff_index();
        assert_eq!(index.counts["a.rs"], (2, 0));
        let t = &index.threads["a.rs"][&(github::DiffSide::Right, 7)][0];
        assert_eq!(t.root.id, CommentKey::Pending(11));
        assert_eq!(t.root.start_line, Some(5));
        assert_eq!(t.replies[0].id, CommentKey::Pending(12));
    }
}

#[test]
fn rejected_operations_roll_back_only_their_own_effect_and_keep_text() {
    let mut d = discussion();
    let mut posts = posting::Posts::default();
    posts
        .changes
        .begin(change(1, CommentKey::Inline(1), None), Some(&d), None);
    posts.changes.begin(
        change(2, CommentKey::Inline(2), Some("rejected text")),
        Some(&d),
        None,
    );
    posts.changes.finish(
        2,
        &Err(github::ClientError::Rejected("no permission".into())),
    );
    assert_eq!(posts.changes.failed[0].recovery_body, "rejected text");
    assert_eq!(
        bodies(&Projection::new(Some(&d), None, &posts).diff_index()),
        ["body 2", "body 3"]
    );
    d.threads[0].comments[2].body = "external edit".into();
    posts.changes.finish(
        1,
        &Err(github::ClientError::Rejected("delete denied".into())),
    );
    assert_eq!(
        bodies(&Projection::new(Some(&d), None, &posts).diff_index()),
        ["body 1", "body 2", "external edit"]
    );
}

#[test]
fn unknown_results_require_exact_positive_proof_in_the_correct_snapshot_domain() {
    for deleting in [false, true] {
        let mut d = discussion();
        let mut posts = posting::Posts::default();
        let op = change(1, CommentKey::Inline(2), (!deleting).then_some("accepted"));
        posts.changes.begin(op.clone(), Some(&d), None);
        posts.changes.finish(
            1,
            &Err(github::ClientError::Failed("transport lost".into())),
        );
        posts.changes.reconcile(None, None, true);
        assert!(posts.has_uncertain());
        posts.changes.reconcile(Some(&d), None, true);
        assert!(posts.has_uncertain());
        if deleting {
            d.threads[0].comments.retain(|c| c.id != 2);
        } else {
            d.threads[0].comments[1].body = "accepted".into();
        }
        posts.changes.reconcile(Some(&d), None, false);
        assert!(posts.has_uncertain());
        posts.changes.reconcile(Some(&d), None, true);
        assert!(!posts.has_uncertain());
    }
    let mut p = review(vec![draft(10, None)]);
    let mut posts = posting::Posts::default();
    posts
        .changes
        .begin(change(1, CommentKey::Pending(10), None), None, Some(&p));
    posts.changes.finish(
        1,
        &Err(github::ClientError::Failed("transport lost".into())),
    );
    posts.changes.reconcile(None, None, false);
    assert!(posts.has_uncertain());
    p.id += 1;
    p.comments.clear();
    posts.changes.reconcile(None, Some(&p), false);
    assert!(posts.has_uncertain());
    p.id -= 1;
    posts.changes.reconcile(None, Some(&p), false);
    assert!(!posts.has_uncertain());
}

#[test]
fn verified_later_unknown_writes_supersede_only_the_older_protected_target() {
    for deleting in [false, true] {
        let mut d = discussion();
        let mut posts = posting::Posts::default();
        let mut sync = sync::SyncState::default();
        let old = change(1, CommentKey::Inline(2), Some("old accepted"));
        sync.begin_write();
        sync.finish_write(Some(&receipt(&old)));
        let newer = change(
            2,
            CommentKey::Inline(2),
            (!deleting).then_some("new accepted"),
        );
        sync.begin_write();
        posts.changes.begin(newer, Some(&d), None);
        posts
            .changes
            .finish(2, &Err(github::ClientError::Failed("response lost".into())));
        sync.finish_write(None);
        let before = sync.begin_read();
        if deleting {
            d.threads[0].comments.retain(|c| c.id != 2);
        } else {
            d.threads[0].comments[1].body = "new accepted".into();
        }
        // A superseding read invalidates the older one, even if both match.
        let ticket = sync.begin_read();
        assert!(!sync.accept_projected(before, &d, &posts.changes));
        assert!(sync.accept_projected(ticket, &d, &posts.changes));
        posts.changes.reconcile(Some(&d), None, true);
        assert!(!posts.has_uncertain());
    }
}

#[test]
fn unknown_pending_write_proof_cannot_be_overwritten_by_an_older_pending_receipt() {
    let mut p = review(vec![draft(10, None)]);
    let mut posts = posting::Posts::default();
    let old = change(1, CommentKey::Pending(10), Some("old accepted"));
    posts.changes.begin(old.clone(), None, Some(&p));
    posts.changes.finish(1, &Ok(receipt(&old)));
    posts.changes.begin(
        change(2, CommentKey::Pending(10), Some("new accepted")),
        None,
        Some(&p),
    );
    posts
        .changes
        .finish(2, &Err(github::ClientError::Failed("response lost".into())));
    let mut cache_guard = posts.changes.session_protection();
    p.comments[0].body = "new accepted".into();
    posts.changes.reconcile(None, Some(&p), false);
    let previous = github::review_session::ReviewSession {
        pending: Some(review(vec![draft(10, None)])),
        verified: true,
    };
    let mut state = github::review_session::ReviewSession {
        pending: Some(p),
        verified: true,
    };
    let mut cache_state = state.clone();
    cache_guard.reconcile(None, cache_state.pending.as_ref(), false);
    cache_guard.protect_session(&mut cache_state, &previous);
    assert_eq!(
        cache_state.pending.unwrap().comments[0].body,
        "new accepted"
    );
    posts.changes.protect_session(&mut state, &previous);
    assert_eq!(state.pending.unwrap().comments[0].body, "new accepted");
    assert!(!posts.has_uncertain());
}

#[test]
fn deleting_a_parent_does_not_leave_an_unacknowledged_created_reply_barrier_stuck() {
    let mut d = discussion();
    let mut posts = posting::Posts::default();
    let mut sync = sync::SyncState::default();
    let created = MutationReceipt::PublishedCreated(github::PublishedComment {
        id: 2,
        node_id: "node-2".into(),
        body: Some("body 2".into()),
        author: None,
        created_at: None,
        anchor: None,
        in_reply_to_id: Some(1),
        thread_id: None,
        review_id: None,
        review_node_id: None,
    });
    sync.begin_write();
    sync.finish_write(Some(&created));
    let op = change(2, CommentKey::Inline(1), None);
    sync.begin_write();
    posts.changes.begin(op.clone(), Some(&d), None);
    posts.changes.finish(2, &Ok(receipt(&op)));
    sync.finish_write(Some(&receipt(&op)));
    d.threads[0].comments.remove(0);
    let real_thread = d.threads[0].id.clone();
    d.threads[0].id = "wrong thread".into();
    let ticket = sync.begin_read();
    assert!(!sync.accept_projected(ticket, &d, &posts.changes));
    d.threads[0].id = real_thread;
    let ticket = sync.begin_read();
    assert!(sync.accept_projected(ticket, &d, &posts.changes));
}

#[test]
fn pending_receipts_protect_session_mirrors_but_speculation_never_does() {
    use github::review_session::ReviewSession;
    for deleting in [false, true] {
        let previous = ReviewSession {
            pending: Some(review(vec![draft(10, None), draft(11, Some(10))])),
            verified: true,
        };
        let mut posts = posting::Posts::default();
        let op = change(
            1,
            CommentKey::Pending(10),
            (!deleting).then_some("accepted"),
        );
        posts
            .changes
            .begin(op.clone(), None, previous.pending.as_ref());
        let mut state = previous.clone();
        posts.changes.protect_session(&mut state, &previous);
        assert_eq!(
            state.pending.as_ref().unwrap().comments[0].body,
            "draft 10",
            "speculation is not a confirmed receipt"
        );
        posts.changes.finish(1, &Ok(receipt(&op)));
        state.pending.as_mut().unwrap().comments[1].body = "external".into();
        posts.changes.protect_session(&mut state, &previous);
        assert_eq!(
            state
                .pending
                .as_ref()
                .unwrap()
                .comments
                .last()
                .unwrap()
                .body,
            "external"
        );
        if deleting {
            assert_eq!(state.count(), 1);
        } else {
            assert_eq!(state.pending.as_ref().unwrap().comments[0].body, "accepted");
        }
        posts.changes.reconcile(None, state.pending.as_ref(), false);
    }
}
