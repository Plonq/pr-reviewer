use super::*;
use github::{
    review_session::Command, Author, CommentAnchor, DiffSide, MutationReceipt,
    PendingCommentReceipt, PublishedComment,
};

fn anchor() -> CommentAnchor {
    CommentAnchor {
        commit_id: "head".into(),
        path: "a.rs".into(),
        side: DiffSide::Right,
        line: 2,
        start_line: Some(1),
        position: None,
    }
}

fn discussion(ids: &[u64]) -> PrDiscussion {
    let mut discussion = PrDiscussion::from_review_comments(
        ids.iter()
            .map(|&id| github::ReviewComment {
                id,
                path: "a.rs".into(),
                side: Some("RIGHT".into()),
                line: Some(2),
                start_line: Some(1),
                body: "identical".into(),
                user: Author {
                    login: "alice".into(),
                },
                created_at: format!("2026-01-{id:02}T00:00:00Z"),
                in_reply_to_id: None,
            })
            .collect(),
    );
    for thread in &mut discussion.threads {
        for comment in &mut thread.comments {
            comment.permissions.node_id = format!("node-{}", comment.id);
        }
    }
    discussion
}

fn published(id: u64, parent: Option<u64>) -> MutationReceipt {
    MutationReceipt::PublishedCreated(PublishedComment {
        id,
        node_id: format!("node-{id}"),
        body: Some("identical".into()),
        author: Some("alice".into()),
        created_at: None,
        anchor: Some(anchor()),
        in_reply_to_id: parent,
        thread_id: None,
        review_node_id: None,
        review_id: None,
    })
}

fn begin(posts: &mut posting::Posts, token: u64, pending: bool, parent: Option<u64>) {
    let command = match (pending, parent) {
        (_, Some(comment_id)) => Command::Reply {
            comment_id,
            body: "identical".into(),
        },
        (true, None) => Command::Add {
            anchor: anchor(),
            body: "identical".into(),
        },
        (false, None) => Command::Immediate {
            anchor: anchor(),
            body: "identical".into(),
        },
    };
    posts.begin_create(
        token,
        command,
        Some(anchor()),
        pending,
        "alice".into(),
        "identical".into(),
        parent,
    );
}

#[test]
fn deleting_a_locally_created_root_keeps_its_created_thread_shell_for_surviving_replies() {
    let mut posts = posting::Posts::default();
    begin(&mut posts, 7, false, None);
    let mut confirmed = discussion(&[11]);
    confirmed.threads[0]
        .comments
        .push(discussion(&[12]).threads[0].comments[0].clone());
    posts.finish_create(7, &Ok(published(11, None)));
    posts.reconcile(&confirmed);
    let change = super::mutations::Change {
        token: 8,
        key: CommentKey::Inline(11),
        target: github::MutationTarget::Published(github::CommentTarget::ReviewComment(
            "node-11".into(),
        )),
        body: None,
        recovery_body: "identical".into(),
        placement: crate::ComposerPlacement::Diff,
        uncertain: false,
        error: None,
    };
    posts.changes.begin(change.clone(), Some(&confirmed), None);
    for saved in [false, true] {
        if saved {
            confirmed.threads[0].comments.remove(0);
            posts.changes.finish(
                8,
                &Ok(MutationReceipt::Deleted {
                    target: change.target.clone(),
                }),
            );
            posts.reconcile(&confirmed);
        }
        let view = Projection::new(Some(&confirmed), None, &posts);
        assert!(
            matches!(view.conversation().as_slice(), [Entry::CreatedThread(post, comments)] if post.token == 7 && comments.len() == 1 && matches!(comments[0], Comment::Published(c) if c.id == 12))
        );
        assert_eq!(view.diff_index().counts["a.rs"], (1, 0));
    }
}

#[test]
fn accepted_unfetched_replies_keep_thread_membership_after_root_deletion() {
    let mut confirmed = discussion(&[11]);
    let thread_id = confirmed.threads[0].id.clone();
    let mut posts = posting::Posts::default();
    begin(&mut posts, 9, false, Some(11));
    posts.finish_create(9, &Ok(published(12, Some(11))));
    let change = super::mutations::Change {
        token: 10,
        key: CommentKey::Inline(11),
        target: github::MutationTarget::Published(github::CommentTarget::ReviewComment(
            "node-11".into(),
        )),
        body: None,
        recovery_body: "identical".into(),
        placement: crate::ComposerPlacement::Diff,
        uncertain: false,
        error: None,
    };
    posts.changes.begin(change.clone(), Some(&confirmed), None);
    for saved in [false, true] {
        if saved {
            confirmed.threads[0].comments.clear();
            posts.changes.finish(
                10,
                &Ok(MutationReceipt::Deleted {
                    target: change.target.clone(),
                }),
            );
            posts.reconcile(&confirmed);
        }
        let view = Projection::new(Some(&confirmed), None, &posts);
        assert_eq!(view.remote_id(CommentKey::Post(9)), Some(12));
        let index = view.diff_index();
        assert_eq!(index.counts["a.rs"], (1, 0));
        assert_eq!(
            index.thread_roots[&thread_id],
            CommentKey::Post(9),
            "known receipt IDs are not necessarily present in a read yet"
        );
        assert!(
            matches!(view.conversation().as_slice(), [Entry::Thread(thread)] if matches!(thread.comments.as_slice(), [Comment::Posting(post)] if post.token == 9))
        );
    }
}

#[test]
fn public_roots_bind_in_place_without_moving_into_a_new_review_wrapper() {
    let mut posts = posting::Posts::default();
    let mut confirmed = discussion(&[]);
    begin(&mut posts, 7, false, None);
    let view = Projection::new(Some(&confirmed), None, &posts);
    assert_eq!(
        view.diff_index().threads["a.rs"][&(DiffSide::Right, 2)][0]
            .root
            .id,
        CommentKey::Post(7)
    );
    assert!(
        matches!(view.conversation().as_slice(), [Entry::CreatedThread(post, _)] if post.token == 7)
    );
    assert!(
        confirmed.threads.is_empty(),
        "projection never mutates confirmed inputs"
    );
    posts.finish_create(7, &Ok(published(11, None)));
    posts.reconcile(&confirmed);
    assert_eq!(
        posts.pending.len(),
        1,
        "successful creation survives a stale read"
    );
    confirmed = discussion(&[11]);
    confirmed.threads[0].comments[0].review_id = Some("review".into());
    confirmed.comments.push(DiscussionComment {
        id: 99,
        permissions: Default::default(),
        author: "alice".into(),
        body: "".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        reactions: vec![],
        review_state: Some("COMMENTED".into()),
        review_id: Some("review".into()),
    });
    posts.reconcile(&confirmed);
    let view = Projection::new(Some(&confirmed), None, &posts);
    assert!(posts.pending.is_empty());
    assert_eq!(
        view.diff_index().threads["a.rs"][&(DiffSide::Right, 2)][0]
            .root
            .id,
        CommentKey::Post(7)
    );
    assert!(
        matches!(view.conversation().as_slice(), [Entry::CreatedThread(post, comments)] if post.token == 7 && matches!(comments[0], Comment::Published(_)))
    );
    assert_eq!(view.remote_id(CommentKey::Post(7)), Some(11));
    assert_eq!(
        view.key(Comment::Published(&confirmed.threads[0].comments[0]), true)
            .markdown_key(),
        "post-7"
    );
    confirmed.threads.clear();
    posts.reconcile(&confirmed);
    assert!(
        Projection::new(Some(&confirmed), None, &posts)
            .conversation()
            .is_empty(),
        "later confirmed deletion cannot resurrect the shell"
    );
}

#[test]
fn pending_roots_and_replies_project_without_fabricating_a_review_and_keep_their_keys() {
    for parent in [None, Some(11)] {
        let confirmed = discussion(&[11]);
        let mut posts = posting::Posts::default();
        begin(&mut posts, 8, true, parent);
        let view = Projection::new(Some(&confirmed), None, &posts);
        assert_eq!(view.pending_count(), 1);
        assert_eq!(view.diff_index().counts["a.rs"], (2, 0));
        if parent.is_some() {
            assert!(
                matches!(&view.conversation()[0], Entry::Thread(t) if matches!(t.comments[1], Comment::Posting(p) if p.pending))
            );
        } else {
            assert_eq!(
                view.conversation().len(),
                1,
                "pending roots remain in Changes/tray, not public Conversation"
            );
        }
        let comment = PendingComment {
            id: 22,
            node_id: "node-22".into(),
            path: "a.rs".into(),
            side: DiffSide::Right,
            line: Some(2),
            start_line: Some(1),
            body: "identical".into(),
            in_reply_to_id: parent,
        };
        let receipt = MutationReceipt::PendingCreated {
            review_id: 42,
            review_node_id: "real-review".into(),
            created: PendingCommentReceipt {
                comment: comment.clone(),
                thread_id: Some("real-thread".into()),
            },
        };
        posts.finish_create(8, &Ok(receipt));
        let review = PendingReview {
            id: 42,
            node_id: "real-review".into(),
            commit_id: Some("dead10cc".into()),
            body: "".into(),
            comments: vec![comment],
        };
        let view = Projection::new(Some(&confirmed), Some(&review), &posts);
        assert!(posts.pending.is_empty());
        assert_eq!(view.pending_count(), 1);
        let index = view.diff_index();
        let comments: Vec<_> = index.threads["a.rs"][&(DiffSide::Right, 2)]
            .iter()
            .flat_map(|t| std::iter::once(&t.root).chain(&t.replies))
            .collect();
        let created = comments
            .iter()
            .find(|c| c.id == CommentKey::Post(8))
            .unwrap();
        assert_eq!(created.author, "alice");
        assert_eq!(
            view.key(Comment::Pending(&review.comments[0]), true),
            CommentKey::Post(8)
        );
        assert_eq!(view.remote_id(CommentKey::Post(8)), Some(22));
        assert_eq!(
            posts.alias(CommentKey::Inline(22)),
            CommentKey::Post(8),
            "publication of the same draft identity retains the alias"
        );
    }
}

#[test]
fn identical_roots_reconcile_independently_without_changing_submission_order() {
    let mut posts = posting::Posts::default();
    for token in [1, 2] {
        begin(&mut posts, token, false, None);
        posts.finish_create(token, &Ok(published(token + 10, None)));
    }
    let mut confirmed = discussion(&[12]);
    posts.reconcile(&confirmed);
    let view = Projection::new(Some(&confirmed), None, &posts);
    assert_eq!(view.diff_index().counts["a.rs"], (2, 0));
    let tokens: Vec<_> = view
        .conversation()
        .iter()
        .map(|entry| match entry {
            Entry::CreatedThread(post, _) => post.token,
            _ => panic!("local shell"),
        })
        .collect();
    assert_eq!(tokens, [1, 2]);
    confirmed.threads.extend(discussion(&[11]).threads);
    posts.reconcile(&confirmed);
    assert!(posts.pending.is_empty());
    assert_eq!(posts.alias(CommentKey::Inline(11)), CommentKey::Post(1));
    assert_eq!(posts.alias(CommentKey::Inline(12)), CommentKey::Post(2));
}

#[test]
fn rejected_and_unknown_creates_preserve_text_but_never_match_by_body() {
    let mut posts = posting::Posts::default();
    begin(&mut posts, 1, true, None);
    begin(&mut posts, 2, false, None);
    posts.finish_create(1, &Err(github::ClientError::Rejected("validation".into())));
    assert_eq!(posts.failed[0].body, "identical");
    assert_eq!(
        posts.pending.len(),
        1,
        "rejection removes only its own overlay"
    );
    posts.finish_create(
        2,
        &Err(github::ClientError::Failed(
            "connection closed after send".into(),
        )),
    );
    assert!(posts.has_uncertain());
    let confirmed = discussion(&[11]);
    posts.reconcile(&confirmed);
    assert_eq!(
        posts.pending.len(),
        1,
        "an identical server body cannot acknowledge an unknown identity"
    );
    assert_eq!(posts.remote_id(CommentKey::Post(2)), None);
    assert_eq!(
        Projection::new(Some(&confirmed), None, &posts)
            .diff_index()
            .counts["a.rs"],
        (2, 0)
    );
}

#[test]
fn scope_filtering_runs_after_creation_projection() {
    let mut posts = posting::Posts::default();
    let mut left = anchor();
    left.side = DiffSide::Left;
    posts.begin_create(
        1,
        Command::Immediate {
            anchor: left.clone(),
            body: "body".into(),
        },
        Some(left),
        false,
        "alice".into(),
        "body".into(),
        None,
    );
    let mut index = Projection::new(None, None, &posts).diff_index();
    assert!(index.threads["a.rs"].contains_key(&(DiffSide::Left, 2)));
    index.hide_left_anchors();
    assert!(index.threads.is_empty());
    assert_eq!(index.counts["a.rs"], (0, 1));
}
