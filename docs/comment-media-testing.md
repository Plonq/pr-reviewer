# Comment media uploads

## Deterministic checks

```sh
cargo test -p gander comment_media --locked
cargo test -p gander media_uploads_block --locked
cargo test --workspace --locked
```

The tests exercise the real Editor paste hook and external-file drop dispatch,
plain-text fallback, multiple attachments with partial failure, UTF-16 placeholder
replacement, typing during an upload, removed/restored placeholders, draft
replacement, and comment/review submission guards. A local HTTP fixture checks
raw upload bytes, authentication headers, refused redirects, and permission
errors. They do not require authentication or upload anything to GitHub.

## Manual authenticated checks

Use a disposable PR in a repository you can write to. Uploads create persistent
GitHub assets even if you cancel the draft; do not use sensitive media in a public
repository. These checks are deliberately not part of the automated suite.

1. Sign in using `gh auth login` and open the PR in Gander.
2. In the conversation composer, type some prose (including an emoji), then paste
   a screenshot copied to the clipboard. Type more while it uploads. Confirm the
   placeholder is replaced at its original position, surrounding text and cursor
   are preserved, and the posted image renders on GitHub and in Gander.
3. Copy an image file in Finder and paste it. Confirm it uploads once, not as a
   local path. Paste ordinary text and a URL; confirm normal text insertion.
4. Drop multiple images and an MP4/MOV/WebM file into the editor. Confirm image
   Markdown and a standalone video URL appear in the drop order. Post and confirm
   the video renders on GitHub and in Gander.
5. Repeat in a new inline comment, a thread reply, an edited comment, a pending
   comment, and the Finish review summary.
6. During a slow upload, attempt both the submit button and Cmd+Enter. Neither
   should submit the placeholder. Editing text should still work. Remove the
   placeholder while uploading; completion must not restore it. Cancel and open
   another draft or PR; the late result must not appear there.
7. Undo an attachment replacement. If this restores an upload placeholder, submit
   remains disabled until it is removed (with an explanation below the field).
8. Drop a directory, an unsupported file, an empty file, an image over 10 MB, and
   a video over 100 MB. Confirm visible errors, preserved prose, and no stuck
   uploading state. Repeat with a network failure or a read-only repository.
   Paste/drop again to retry; successful files in a mixed batch must remain.
9. Check a private repository with non-sensitive media: attachment URLs should
   use `https://github.com/user-attachments/assets/…`, not a public gist or a
   release asset. Verify rendering with the existing authenticated media bridge.
10. Open a local repository review. Pasting ordinary text must still work; no
    GitHub attachment uploads are enabled there.

## Upload protocol

The implementation follows GitHub CLI's attachment protocol: resolve the numeric
repository ID with `gh api`, obtain the current credential with `gh auth token`,
then POST raw bytes to `https://uploads.github.com/user-attachments/assets` with
`name`, `content_type`, and `repository_id` query parameters. The endpoint is not
a versioned public REST API and may change. GitHub can reject credentials, plans,
or repository permissions; those failures are shown in the composer. Uploads
have a three-minute HTTP timeout, refuse redirects, and never fall back to other
hosting. Credentials are not persisted by this feature or put in URLs.

Reference implementation: [GitHub CLI v2.102.0 attachment client](https://github.com/cli/cli/blob/v2.102.0/internal/attachments/client.go).
