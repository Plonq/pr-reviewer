# Optimistic inline comments: investigation and implementation plan

Status: proposed architecture; no runtime behavior changes yet.

## Scope and recommendation

Make inline roots and replies optimistic for creation, editing, and deletion,
including pending-review drafts, regardless of whether the action starts in
Changes, Conversation, or the pending-review tray. Local-only comments already
update immediately and should keep their existing persistence semantics.

Start with a focused comment-state refactor, not separate optimistic patches to
each handler. Keep GitHub's review lifecycle and API positioning logic intact.
Do not introduce a general-purpose mutation framework or a durable offline queue.

## What exists today

- `crates/app/src/main.rs::submit_composer` has separate paths for published
  edits, pending edits, Conversation posts/replies, pending diff comments/replies,
  local comments, and immediate diff comments. The last path even bypasses
  `run_session`. Editors normally remain until the write finishes.
- `crates/app/src/conversation/posting.rs::Posts` already provides transient
  Conversation placeholders, exact receipt matching, and stable Markdown keys
  across placeholder replacement. It covers creation, not edits/deletes, and is
  not part of the diff's comment index. Preserve these guarantees when replacing
  it; do not match posts by body text.
- `crates/app/src/comment_actions.rs::apply_comment_mutation` applies published
  edits/deletes only after success, directly to the discussion and diff index.
- Published discussion, pending session, and diff index are separate structures.
  Conversation additionally appends transient posts and pending replies itself.
  A new optimistic representation in just one collection will not update every
  surface, badge, or count.
- `crates/github/src/review_session.rs` re-reads the pending review before draft
  mutations. A reply intended as published can become pending if a review was
  started elsewhere. Partial success is possible: starting a review can succeed
  while adding its comment fails. Keep these semantics.
- `execute_with_receipt` returns only `Option<u64>`. Immediate anchored comments
  discard their result; ordinary `Command::Reply` also discards its reply ID.
  `ConversationReply` retains a published reply ID. Pending creates do return
  their full comment through the resulting session, but without an explicit
  operation-to-comment receipt.
- `refetch_comments` and full `ReviewItem::install` both replace comment data.
  The discussion generation protects against a comments fetch superseded by a
  full install, but does not order two comments-only fetches in that generation
  or provide a mutation barrier against pre-write snapshots.
- Diff Markdown measurement caches use numeric comment IDs in
  `crates/app/src/diff_media.rs`. Conversation keys also affect persistent media
  state. Changing an optimistic ID to a server ID can remount or remeasure content.
- Conversation groups threads under their owning submitted review. Therefore
  retaining only the comment element's key is insufficient: its ancestor,
  ordering, and thread/review identity also matter.

## Proposed state boundary

Introduce an app-owned comment model (for example `crates/app/src/comments/`)
with a pure projection/reconciliation core and a thin GPUI execution adapter:

```text
confirmed GitHub discussion + confirmed pending review
                        +
       transient comment operations + identity aliases
                        |
                        v
                projected comment view
                  /         |         \
          Conversation   diff index   pending tray/counts
```

Initially keep the existing storage owners (`ItemData.discussion` and
`SessionUi.state`); the projection can take references to both. There is no need
to move all PR state or redesign the store to establish this boundary.

The projected view is app-owned rather than a fake `github::PrDiscussion`:

- Stable, typed UI comment and thread keys are distinct from remote numeric IDs
  and GraphQL mutation targets. Allocate a local key for a new comment; alias the
  returned server identity to that same key for the lifetime of the open item.
  Existing comments can use namespaced remote keys. Never reserve an arbitrary
  high-number range of GitHub IDs for temporary comments.
- Include body, author, anchor, reply/thread membership, draft/publication state,
  permissions, reactions, display time, stable placement, and sync status.
  Do not encode `Posting…` as a timestamp or treat an uploading draft as published.
- Overlay operations are create-root, create-reply, edit, and delete, each with
  an operation ID, target/key, submitted payload, and completion state. Editing
  replaces the projected body; deletion hides the target. Do not snapshot and
  later restore an entire PR on failure.
- Keep recovery text and operation errors independently of the visible composer.
  Successful operations must finish even after navigation or editor closure.
- Project first, then apply existing comparison-scope anchor filtering and row
  building. Conversation, diff rows, tray, and badges must consume that same
  logical result; none should append an additional placeholder layer.

## Mutation lifecycle

1. Validate body/anchor, uploads, permissions, and operation availability locally.
2. Register the operation synchronously, project all surfaces, and close the
   submitted editor in that same UI update. This is the intentional layout
   change. For deletion, retain the existing confirmation prompt, then remove
   the card immediately after confirmation.
3. Execute asynchronously using the existing GitHub lifecycle rules. Initially
   retain the per-PR single-flight policy; optimistic feedback does not require
   concurrent writes. Disable conflicting actions rather than inventing a queue
   or allowing requests against an unassigned remote ID. Keep navigation usable.
4. On success, bind remote identities and merge returned authoritative fields.
   Retain any overlay needed to protect against stale reads until reconciliation.
   A refresh failure is not a failed write and must never trigger another POST.
5. On definite rejection, remove only that operation's overlay and preserve its
   submitted text with an explicit recovery action. Failed deletes restore the
   card; failed edits show the confirmed body with the rejected text recoverable.
   Do not reopen an editor over another PR or overwrite a newer composer.
6. Treat transport/unknown-outcome failures separately from definite rejection:
   the server may already have committed the write. Reconcile and show an
   unverified state rather than blindly retrying a create. If the backend cannot
   establish identity, offer verification/recovery rather than fuzzy body matching.

Keep confirmed `ReviewSession` state separate from optimistic drafts. In
particular, an optimistic first draft must not fabricate a server review ID or
mark that review verified. Project pending counts/review presence from operations
without passing temporary identities to GitHub. Block review submission while a
comment write or uncertain outcome could make the submitted review ambiguous.

## Backend prerequisite: useful mutation receipts

Replace the lossy receipt contract with a typed outcome describing what happened:
no comment change, published comment created, pending comment created, edited,
or deleted. Return the created comment's identities and available server fields;
include owning thread/review identities where supplied. The resulting confirmed
session remains part of the execution result, including partial failure state.

In `gh_cli.rs`, retain the REST comment response for single-line, multi-line, and
reply creation. For revision-positioned immediate comments, expand the existing
atomic submitted-review GraphQL mutation's selection to return its new thread
and comment identities. Do not replace it with start/add/submit calls: that could
interfere with an unrelated pending review. Add fixture tests for the expanded
response contract before depending on it in the app.

Prefer requiring exact receipts for these production paths over accumulating
optional-ID fallbacks that leave successful placeholders unreconcilable.

## Refresh and reconciliation rules

Route comments-only refreshes, full installs, session completions, and mutation
completions through the same comment-model reconciliation boundary.

- Track request ordering and the mutation revision at fetch start. Discard
  superseded fetch results; pre-write reads cannot acknowledge a later write.
- Reapply unresolved overlays whenever confirmed snapshots change. This prevents
  flickering to the old body or resurrecting a deleted card during refresh.
- Retire a successful overlay only when an eligible post-write snapshot confirms
  its effect: the exact created identity in the right thread/review, the edited
  target with the accepted body/version, or the deleted target's absence.
- Removing an overlay and installing its confirmed replacement is one projection
  update. Keep identity aliases afterward so Markdown and expansion state survive.
- Thread-root deletion must preserve surviving replies and the original thread
  identity/anchor. Pending flat comments need an explicit reparenting policy;
  blindly filtering a root currently risks orphaning its replies in grouping.
- Persist only confirmed data through the existing store APIs. A transient
  overlay or synthetic timestamp must never become an authoritative snapshot.

Request sequencing alone is not proof that a server read is current. Receipts
and positive acknowledgement are also required, especially for edits/deletes.

## Keeping acknowledgement layout-stable

The target is no avoidable geometry change between the optimistic card and the
successful acknowledgement, not between the editor and its submitted result.

- Render the final card/Markdown renderer immediately with the submitted body,
  signed-in author, anchor, and normal reply indentation.
- Preserve comment, thread, and review-wrapper keys, placement, expansion state,
  and Markdown measurement keys. Bind server metadata to those shells instead
  of moving a new root into a different review wrapper after the fetch. Predict
  the wrapper from the command where possible; unexpected server rerouting to a
  pending review must be represented honestly even if it requires a change.
- Keep sync/time metadata single-line and bounded. Reserve the normal action and
  reaction chrome from the first frame so permissions becoming available do not
  insert a new footer. Do not let a timestamp wrap the header.
- For new root threads, provide code context from the known revision/anchor when
  available, or reserve a bounded context region if it will arrive later.
- Reuse the existing pre-paint Markdown measurement and anchored diff rebuilds.
  Test both comment geometry and the viewport anchor, not just element identity.

Content edits, deletion, failure recovery, external comments, and newly loaded
media may legitimately change geometry. A metadata-only acknowledgement should
not. Stable keys alone are not a sufficient layout test.

## Suggested implementation sequence

1. **Behavior-preserving comment projection refactor.** Extract comment lookup,
   merging, grouping, and identity mapping into the app model. Route all three
   surfaces/counts through it. Migrate existing Conversation placeholders without
   losing exact matching, media identity, or confirmed-only caching. Keep current
   submit/close timing until the next stages. Add projection parity tests.
2. **Typed receipts and refresh barriers.** Unify immediate inline execution with
   the completion path; cover all anchored/reply API variants and full refreshes.
3. **Optimistic inline creation/replies.** Register overlays and close editors
   immediately for published and pending comments; bind receipts in place.
4. **Optimistic editing/deleting.** Add body overlays/tombstones, root-deletion
   handling, and recovery for rejection/unknown outcomes.
5. **Layout and race validation.** Exercise the matrix below before broadening
   optimism to other features. No global state-management rewrite is necessary.

## Validation matrix

- Roots/replies, published/pending, unified/split, Conversation/Changes/tray.
- Assert the projected result and editor closure *before* releasing a blocked
  fake GitHub response, then verify exactly one write and stable identity.
- Identical repeated bodies reconcile independently by receipt, not text.
- Slow/stale/failed reads after successful writes retain the optimistic result;
  failed refreshes do not reopen composers or retry writes.
- Reads begun before a mutation, out-of-order comments reads, and full refresh
  racing a write cannot undo the projection.
- Definite failure preserves text; unknown-outcome failure does not duplicate
  creation. Partial start-review success preserves the real pending review.
- Navigating files/PRs, closing a composer/item, and opening a newer editor do not
  misroute completion or steal focus.
- Deleting roots with replies retains the surviving thread; counts match all
  surfaces. Resolved/outdated threads retain expansion and historical anchors.
- Compare card bounds, following-card bounds, row counts, scroll anchors, and
  Markdown/media keys before/after metadata-only acknowledgement, including
  permissions/reactions and review grouping becoming available.
- Confirmed snapshots/session mirrors exclude speculative data; existing review
  lifecycle, permissions, attachment-upload gating, and local comments regressions
  remain covered.

Existing `conversation::posting::tests` and the GPUI placeholder-in-place tests
are useful starting points. Most projection/race cases should be pure tests;
retain GPUI visual tests for bounds, focus, composer replacement, and scrolling.

### Baseline checks run during investigation

All eight selected tests passed:

```sh
cargo test -p gander --locked conversation::posting::tests
cargo test -p gander --locked placeholder_
cargo test -p gander --locked published_inline_edit_saves_in_both_views_without_touching_the_pending_review
cargo test -p gander --locked range_drafts_resume_edit_delete_and_submit_without_posting_single_comments
```

These validate existing behavior, not the proposed architecture. The full suite
was not run. The build emitted the existing unused `mark_thread_seen` warning
and a dependency future-incompatibility warning for `block`.
