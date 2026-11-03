# Testing embedded video on macOS

Automated coverage:

```sh
cargo test --workspace --locked
cargo build -p gander --all-targets --locked
```

The application tests use the real Markdown hosts, diff row measurement and
modal event handlers, with a deterministic pending downloader in GUI tests.
Separate real-socket HTTP tests cover HEAD metadata, redirect authentication,
streaming bounds, progress, cancellation, atomic files and permissions. They do
not require GitHub credentials or download public media.

## Native decoding probe

A small diagnostic window exercises the same `VideoPlayer` and GPUI CoreVideo
surface as the application, using a local file only:

```sh
cargo run -p gander --example video_probe --locked -- /absolute/clip.mp4 --verify
cargo run -p gander --example video_probe --locked -- /absolute/portrait.mov --verify
cargo run -p gander --example video_probe --locked -- /absolute/clip.webm
```

Running this command is an explicit playback request. The diagnostic plays the
file, prints the first decoded frame's dimensions and duration, and exits after
15 seconds (or earlier for an unsupported format). `--verify` pauses, checks for
position drift, seeks to three seconds, checks mute/volume state, and resumes;
use a fixture at least five seconds long. Space, Left/Right, M and Esc are also
available.

For reproducible public fixtures, Samplelib provides
[MP4](https://samplelib.com/sample-mp4.html) and
[WebM](https://samplelib.com/sample-webm.html) clips. Use a phone portrait MOV or
an independently rotated MOV to check orientation, not just a landscape file
with a different extension. Fixtures are deliberately not committed.

The Metal surface requires full-range bi-planar NV12 (`420f`). The backend
requests that format and checks it before handing a frame to GPUI. The surface
shader also uses a fixed BT.601 YCbCr conversion, so composition explicitly
requests that output matrix (with SDR BT.709 primaries/transfer) and rejects a
different decoded matrix. GPUI's surface vertex shader clips against its
content mask; the app keeps the normal discussion-pane clipping. Composition
preparation is asynchronous: a synchronous AVFoundation composition call can
block on an unsupported WebM. The composition applies the preferred track
transform. Cache filenames retain a container suffix derived from response
metadata; AVFoundation can reject an otherwise valid local MP4 with no suffix.

## Manual review workflow checklist

Use a PR containing authorized test attachments; do not post or alter the PR.

- Check MP4 and portrait MOV playback, sound, correct orientation/aspect ratio,
  seeking, end/replay, mute and volume. Repeat with the app already fullscreen.
- Check private extensionless GitHub attachments while signed in through `gh`.
  Discovery should use metadata only; the full GET starts only after Play.
- Expand while playing and return inline while paused and while playing. Check
  position/audio continuity, previous focus, review scroll and selection.
- Repeat in conversation comments and inline review threads, unified and split,
  including partially virtualized cards and window resizing. Frames must remain
  clipped to the discussion pane and must not cover adjacent controls.
- Start another card, leave the originating pane, close its review, and quit.
  Check that audio stops/pauses as appropriate and local session files disappear
  when released. Returning to a pane must not auto-resume.
- Check slow downloads, Cancel/Retry, an HTTP error/login response, oversized
  media and unsupported WebM. Errors must remain in the card; browser opening
  must always require an explicit click.

The format boundary is system-supported codecs, not arbitrary WebM. A WebM
failure is an expected unsupported-format result, not a reason to silently add
a bundled decoder or webview. Audio-control readbacks in the probe do **not**
prove audible output, and fake-platform GUI tests do not prove native pixels;
those require the manual checks above.
