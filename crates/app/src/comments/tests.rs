//! Projection parity, independent of GPUI and backend execution.
use super::*;
use index::{CommentSide, CommentTime};

fn published(id: u64, parent: Option<u64>, line: Option<u64>) -> github::ReviewComment {
    github::ReviewComment {
        id,
        path: "a.rs".into(),
        side: Some("RIGHT".into()),
        line,
        start_line: None,
        body: format!("published {id}"),
        user: github::Author {
            login: "alice".into(),
        },
        created_at: format!("2026-01-01T00:00:{id:02}Z"),
        in_reply_to_id: parent,
    }
}

fn draft(id: u64, parent: Option<u64>, line: Option<u64>) -> PendingComment {
    PendingComment {
        id,
        node_id: format!("draft-{id}"),
        path: "a.rs".into(),
        side: CommentSide::Right,
        line,
        start_line: None,
        body: format!("pending {id}"),
        in_reply_to_id: parent,
    }
}

fn pending(comments: Vec<PendingComment>) -> PendingReview {
    PendingReview {
        id: 100,
        node_id: "review-100".into(),
        body: String::new(),
        comments,
    }
}

fn comment(id: u64) -> DiscussionComment {
    DiscussionComment {
        id,
        author: "alice".into(),
        body: "same text".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        permissions: Default::default(),
        reactions: vec![],
        review_state: None,
        review_id: None,
    }
}

#[test]
fn published_and_pending_roots_replies_dedup_and_counts_share_one_projection() {
    let discussion = PrDiscussion::from_review_comments(vec![
        published(1, None, Some(7)),
        published(2, Some(1), Some(7)),
        published(3, Some(2), Some(7)),
        published(4, Some(1), Some(7)),
        published(20, None, None),
        published(21, Some(20), None),
    ]);
    let review = pending(vec![
        draft(4, Some(1), Some(7)),
        draft(10, None, Some(9)),
        draft(11, Some(10), Some(9)),
    ]);
    let posts = posting::Posts::default();
    let view = Projection::new(Some(&discussion), Some(&review), &posts);
    assert_eq!(view.pending.len(), 3);
    assert_eq!(view.pending_comment(4).unwrap().body, "pending 4");
    assert!(matches!(view.inline(4), Some(Comment::Pending(c)) if c.body == "pending 4"));
    let index = view.diff_index();
    assert_eq!(index.counts["a.rs"], (6, 2));
    let thread = &index.threads["a.rs"][&(CommentSide::Right, 7)][0];
    assert_eq!(thread.root.id, 1);
    assert_eq!(
        thread.replies.iter().map(|c| c.id).collect::<Vec<_>>(),
        [2, 3, 4]
    );
    assert_eq!(thread.replies[2].body, "pending 4");
    assert_eq!(thread.replies[2].time, CommentTime::Pending);
    let pending_root = &index.threads["a.rs"][&(CommentSide::Right, 9)][0];
    assert_eq!(pending_root.root.id, 10);
    assert_eq!(pending_root.replies[0].id, 11);
    assert_eq!(pending_root.root.author, "you");
    // Conversation keeps its existing visibility policy: pending roots are in
    // Changes/the tray, pending replies to published roots are in Conversation.
    let entries = view.conversation();
    let thread = entries
        .iter()
        .find_map(|e| match e {
            Entry::Thread(t) if t.confirmed.comments[0].id == 1 => Some(t),
            _ => None,
        })
        .unwrap();
    assert_eq!(thread.comments.len(), 4);
    assert!(matches!(thread.comments[3], Comment::Pending(c) if c.id == 4));
}

#[test]
fn pending_duplicates_are_not_counted_or_grouped_twice() {
    let review = pending(vec![draft(10, None, Some(9)), draft(10, None, Some(9))]);
    let discussion = PrDiscussion::from_review_comments(vec![published(10, None, Some(9))]);
    let posts = posting::Posts::default();
    let view = Projection::new(Some(&discussion), Some(&review), &posts);
    assert_eq!(view.pending.len(), 1);
    let index = view.diff_index();
    assert_eq!(index.counts["a.rs"], (1, 0));
    assert_eq!(
        index.threads["a.rs"][&(CommentSide::Right, 9)][0].root.body,
        "pending 10"
    );
    assert!(
        view.conversation().is_empty(),
        "a pending root is not a public thread shell"
    );
}

#[test]
fn transient_posts_are_projected_once_without_expanding_diff_optimism() {
    let discussion = PrDiscussion::from_review_comments(vec![published(1, None, Some(7))]);
    let mut posts = posting::Posts::default();
    posts.begin(1, None, "you".into(), "top level".into());
    posts.begin(2, Some(1), "you".into(), "reply".into());
    let view = Projection::new(Some(&discussion), None, &posts);
    let entries = view.conversation();
    assert_eq!(entries.len(), 2);
    assert!(
        matches!(&entries[0], Entry::Thread(t) if t.comments.len() == 2
        && matches!(t.comments[1], Comment::Posting(p) if p.token == 2))
    );
    assert!(matches!(entries[1], Entry::Comment(Comment::Posting(p)) if p.token == 1));
    assert!(
        view.pending.is_empty(),
        "published placeholders do not fabricate a pending review"
    );
    assert_eq!(
        view.diff_index().counts["a.rs"],
        (1, 0),
        "no new diff optimism in step 1"
    );
}

#[test]
fn receipt_reconciliation_preserves_comment_thread_review_and_markdown_identity() {
    let mut discussion = PrDiscussion::from_review_comments(vec![published(1, None, Some(7))]);
    discussion.threads[0].id = "thread-shell".into();
    discussion.threads[0].comments[0].review_id = Some("review-shell".into());
    let mut review = comment(99);
    review.review_state = Some("COMMENTED".into());
    review.review_id = Some("review-shell".into());
    review.body.clear();
    discussion.comments.push(review);
    let mut posts = posting::Posts::default();
    posts.begin(1, Some(1), "alice".into(), "same text".into());
    let before = Projection::new(Some(&discussion), None, &posts);
    let key = before.key(Comment::Posting(&posts.pending[0]), true);
    let thread_key = match &before.conversation()[0] {
        Entry::Review(_, threads) => {
            assert_eq!(threads.len(), 1);
            threads[0].key.clone()
        }
        _ => panic!("thread remains under its submitted review"),
    };
    posts.complete(1, Some(2));
    discussion.threads[0].comments.push(comment(2));
    posts.reconcile(&discussion);
    let after = Projection::new(Some(&discussion), None, &posts);
    assert!(posts.pending.is_empty());
    let loaded = after.inline(2).unwrap();
    assert_eq!(after.key(loaded, true), key);
    assert_eq!(after.key(loaded, true).markdown_key(), "post-1");
    assert_ne!(
        after.key(loaded, false),
        key,
        "issue/inline namespaces cannot collide"
    );
    match &after.conversation()[0] {
        Entry::Review(c, threads) => {
            assert_eq!(c.review_id.as_deref(), Some("review-shell"));
            assert_eq!(threads[0].key, thread_key);
            assert_eq!(threads[0].comments.len(), 2);
        }
        _ => panic!("receipt must not change thread placement"),
    }
}

#[test]
fn lookup_and_keys_keep_permissions_and_remote_namespaces_separate() {
    let mut discussion = PrDiscussion::from_review_comments(vec![published(1, None, Some(7))]);
    discussion.threads[0].comments[0].permissions.node_id = "inline-node".into();
    discussion.threads[0].comments[0].permissions.can_edit = true;
    let mut issue = comment(1);
    issue.permissions.node_id = "issue-node".into();
    discussion.comments.push(issue);
    let posts = posting::Posts::default();
    let view = Projection::new(Some(&discussion), None, &posts);
    let inline = view
        .published(&github::CommentTarget::ReviewComment("inline-node".into()))
        .unwrap();
    assert!(inline.permissions.can_edit);
    assert!(std::ptr::eq(inline, &discussion.threads[0].comments[0]));
    let issue = view
        .published(&github::CommentTarget::Issue("issue-node".into()))
        .unwrap();
    assert_eq!(
        view.key(Comment::Published(issue), false),
        CommentKey::Issue(1)
    );
    assert_eq!(
        view.key(Comment::Published(inline), true),
        CommentKey::Inline(1)
    );
    assert_ne!(
        view.key(Comment::Pending(&draft(1, None, Some(7))), true),
        CommentKey::Inline(1)
    );
    assert!(view
        .published(&github::CommentTarget::Issue("inline-node".into()))
        .is_none());
}

#[test]
fn scope_filtering_is_after_projection_and_never_changes_confirmed_snapshots() {
    let mut raw = published(1, None, Some(7));
    raw.side = Some("LEFT".into());
    let discussion = PrDiscussion::from_review_comments(vec![raw]);
    let mut draft = draft(10, None, Some(9));
    draft.side = CommentSide::Left;
    let review = pending(vec![draft]);
    let discussion_before = serde_json::to_value(&discussion).unwrap();
    let review_before = serde_json::to_value(&review).unwrap();
    let mut posts = posting::Posts::default();
    posts.begin(1, Some(1), "alice".into(), "not cached".into());
    let view = Projection::new(Some(&discussion), Some(&review), &posts);
    let mut index = view.diff_index();
    index.hide_left_anchors();
    assert!(index.threads.is_empty());
    assert_eq!(index.counts["a.rs"], (0, 2));
    assert_eq!(
        discussion_before,
        serde_json::to_value(&discussion).unwrap()
    );
    assert_eq!(review_before, serde_json::to_value(&review).unwrap());
    assert_eq!(view.pending[0].line, Some(9));
    assert_eq!(
        view.diff_index().counts["a.rs"],
        (2, 0),
        "projection is repeatable and unfiltered"
    );
}
