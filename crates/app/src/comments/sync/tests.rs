use super::*;
use github::{CommentPermissions, DiscussionComment, PublishedComment};

fn comment(id: u64, body: &str) -> DiscussionComment {
    DiscussionComment {
        id,
        permissions: CommentPermissions {
            node_id: format!("IC_{id}"),
            ..Default::default()
        },
        body: body.into(),
        author: "tester".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        reactions: vec![],
        review_id: None,
        review_state: None,
    }
}

fn discussion(ids: &[u64]) -> PrDiscussion {
    PrDiscussion {
        events: vec![],
        comments: ids.iter().map(|id| comment(*id, "identical")).collect(),
        threads: vec![],
    }
}

fn created(id: u64) -> MutationReceipt {
    MutationReceipt::PublishedCreated(PublishedComment {
        id,
        node_id: format!("IC_{id}"),
        body: Some("identical".into()),
        author: None,
        created_at: None,
        anchor: None,
        in_reply_to_id: None,
        thread_id: None,
        review_node_id: None,
        review_id: None,
    })
}

fn complete(sync: &mut SyncState, receipt: &MutationReceipt) {
    sync.begin_write();
    sync.finish_write(Some(receipt));
}

#[test]
fn comments_and_full_reads_share_order_even_when_newest_fails() {
    let mut sync = SyncState::default();
    let comments = sync.begin_read();
    let full = sync.begin_read();
    assert!(!sync.accept(comments, &discussion(&[])));
    assert!(sync.accept(full, &discussion(&[])));
    let latest = sync.begin_read(); // Fails; do not call accept.
    assert!(sync.eligible(latest));
    assert!(
        !sync.accept(full, &discussion(&[])),
        "a failed newer request does not revive an older result"
    );
}

#[test]
fn reads_started_before_or_during_write_cannot_acknowledge_its_receipt() {
    let mut sync = SyncState::default();
    let before = sync.begin_read();
    sync.begin_write();
    assert!(!sync.accept(before, &discussion(&[100])));
    let during = sync.begin_read();
    sync.finish_write(Some(&created(100)));
    assert!(!sync.accept(during, &discussion(&[100])));
    assert_eq!(sync.awaiting.len(), 1);
    let after = sync.begin_read();
    assert!(sync.accept(after, &discussion(&[100])));
    assert!(sync.awaiting.is_empty());
}

#[test]
fn stale_or_failed_post_write_reads_keep_exact_identical_creates_independent() {
    let mut sync = SyncState::default();
    complete(&mut sync, &created(100));
    complete(&mut sync, &created(101));
    let stale = sync.begin_read();
    assert!(!sync.accept(stale, &discussion(&[99, 100])));
    assert_eq!(sync.awaiting.len(), 2);
    let failed = sync.begin_read();
    assert!(sync.eligible(failed)); // A failed read cannot acknowledge either.
    assert_eq!(sync.awaiting.len(), 2);
    let fresh = sync.begin_read();
    assert!(sync.accept(fresh, &discussion(&[99, 100, 101])));
    assert!(sync.awaiting.is_empty());
}

#[test]
fn inline_receipts_require_the_exact_comment_thread_and_review_not_text() {
    let raw = github::ReviewComment {
        id: 11,
        path: "a.rs".into(),
        side: Some("RIGHT".into()),
        line: Some(9),
        start_line: None,
        body: "root".into(),
        user: github::Author {
            login: "tester".into(),
        },
        created_at: "2026-01-01T00:00:00Z".into(),
        in_reply_to_id: None,
    };
    let mut snapshot = PrDiscussion::from_review_comments(vec![raw]);
    snapshot.threads[0].id = "thread".into();
    snapshot.threads[0].comments[0].review_id = Some("review".into());
    let mut reply = comment(100, "identical");
    reply.review_id = Some("review".into());
    snapshot.threads[0].comments.push(reply);
    let MutationReceipt::PublishedCreated(mut receipt) = created(100) else {
        unreachable!()
    };
    receipt.in_reply_to_id = Some(11);
    receipt.thread_id = Some("thread".into());
    receipt.review_node_id = Some("review".into());
    let mut sync = SyncState::default();
    complete(&mut sync, &MutationReceipt::PublishedCreated(receipt));
    for variant in 0..4 {
        let mut stale = snapshot.clone();
        match variant {
            0 => stale.threads[0].id = "other-thread".into(),
            1 => stale.threads[0].comments[1].review_id = Some("other-review".into()),
            2 => stale.threads[0].comments[1].id = 99,
            _ => stale.threads[0].comments[1].permissions.node_id = "other-comment".into(),
        }
        let ticket = sync.begin_read();
        assert!(!sync.accept(ticket, &stale));
    }
    let ticket = sync.begin_read();
    assert!(sync.accept(ticket, &snapshot));
}

#[test]
fn successful_edits_and_deletes_need_positive_acknowledgement() {
    let target = MutationTarget::Published(CommentTarget::Issue("IC_1".into()));
    let mut sync = SyncState::default();
    complete(
        &mut sync,
        &MutationReceipt::Edited {
            target: target.clone(),
            body: "accepted".into(),
        },
    );
    let stale = sync.begin_read();
    assert!(!sync.accept(stale, &discussion(&[1])));
    let missing = sync.begin_read();
    assert!(
        !sync.accept(missing, &discussion(&[])),
        "absence is not acknowledgement of an edit"
    );
    let mut current = discussion(&[1]);
    current.comments[0].body = "accepted".into();
    let fresh = sync.begin_read();
    assert!(sync.accept(fresh, &current));
    complete(&mut sync, &MutationReceipt::Deleted { target });
    let resurrecting = sync.begin_read();
    assert!(!sync.accept(resurrecting, &current));
    let absent = sync.begin_read();
    assert!(sync.accept(absent, &discussion(&[])));
}

#[test]
fn later_accepted_versions_supersede_only_the_same_target() {
    let mut sync = SyncState::default();
    let target = MutationTarget::Published(CommentTarget::Issue("IC_1".into()));
    complete(&mut sync, &created(1));
    complete(&mut sync, &created(2));
    complete(
        &mut sync,
        &MutationReceipt::Edited {
            target: target.clone(),
            body: "first edit".into(),
        },
    );
    complete(
        &mut sync,
        &MutationReceipt::Edited {
            target: target.clone(),
            body: "second edit".into(),
        },
    );
    assert_eq!(sync.awaiting.len(), 2);
    complete(&mut sync, &MutationReceipt::Deleted { target });
    let ticket = sync.begin_read();
    assert!(sync.accept(ticket, &discussion(&[2])));
}

#[test]
fn failed_or_pending_writes_invalidate_reads_without_a_published_effect() {
    let mut sync = SyncState::default();
    let before = sync.begin_read();
    sync.begin_write();
    let during = sync.begin_read();
    sync.finish_write(None); // Includes partial start-review success.
    assert!(!sync.eligible(before));
    assert!(!sync.eligible(during));
    let fresh = sync.begin_read();
    assert!(sync.accept(fresh, &discussion(&[])));
    let pending = MutationReceipt::PendingCreated {
        review_id: 91,
        review_node_id: "review".into(),
        created: github::PendingCommentReceipt {
            comment: github::PendingComment {
                id: 100,
                node_id: "draft".into(),
                path: "a.rs".into(),
                side: github::DiffSide::Right,
                line: Some(1),
                start_line: None,
                body: "draft".into(),
                in_reply_to_id: None,
            },
            thread_id: Some("thread".into()),
        },
    };
    complete(&mut sync, &pending);
    let fresh = sync.begin_read();
    assert!(
        sync.accept(fresh, &discussion(&[])),
        "pending data is confirmed by the session, not a published read"
    );
}
