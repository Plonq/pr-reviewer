use super::*;
use gpui::prelude::*;
use gpui::{div, px, TestAppContext, VisualTestContext};

struct Field {
    input: Entity<EditorState>,
    uploads: Entity<Uploads>,
}

impl Render for Field {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(600.))
            .child(crate::comment_editor::field_with_media(
                &self.input,
                Some(&self.uploads),
                false,
                cx,
            ))
    }
}

fn fixture(cx: &mut TestAppContext, upload: Uploader) -> (VisualTestContext, Entity<Field>) {
    cx.update(gpui::component::init);
    let window = cx.add_window(|window, cx| {
        let input = cx.new(|cx| crate::comment_editor::new(window, cx));
        input.update(cx, |input, cx| input.focus(window, cx));
        let uploads = Uploads::new(
            github::PrLocator {
                owner: "owner".into(),
                repo: "repo".into(),
                number: 1,
            },
            &input,
            cx,
        );
        uploads.update(cx, |uploads, _| uploads.upload = upload);
        Field { input, uploads }
    });
    let field = window.root(cx).unwrap();
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    vcx.update(|window, cx| window.draw(cx).clear(cx));
    (vcx, field)
}

#[gpui::test]
fn pasted_images_upload_in_place_without_overwriting_later_typing(cx: &mut TestAppContext) {
    let (mut vcx, field) = fixture(
        cx,
        Arc::new(|loc, source| {
            assert_eq!(loc.repo, "repo");
            assert!(matches!(source, Source::Image(_)));
            Ok("![image](https://github.com/user-attachments/assets/image)".into())
        }),
    );
    vcx.simulate_input("before 🦀 ");
    vcx.update(|window, cx| {
        cx.write_to_clipboard(ClipboardItem::new_image(&gpui::Image::empty()));
        gpui::test::TestWindowExt::press(window, "cmd-v", cx);
        let field = field.read(cx);
        assert_eq!(field.uploads.read(cx).pending(), 1);
        assert!(blocks_submission(Some(&field.uploads), &field.input, cx));
        assert!(field.input.read(cx).value().contains(MARKER_PREFIX));
        let input = field.input.clone();
        input.update(cx, |input, cx| input.replace("later 🐬", window, cx));
    });
    vcx.run_until_parked();
    vcx.update(|_, cx| {
        let field = field.read(cx);
        let text = field.input.read(cx).value();
        assert_eq!(
            text.as_str(),
            "before 🦀 \n![image](https://github.com/user-attachments/assets/image)\nlater 🐬"
        );
        assert_eq!(
            field.input.read(cx).selected_range(),
            text.len()..text.len()
        );
        assert_eq!(field.uploads.read(cx).pending(), 0);
        assert!(!blocks_submission(Some(&field.uploads), &field.input, cx));
    });
}

#[gpui::test]
fn attachments_use_only_the_line_breaks_needed(cx: &mut TestAppContext) {
    let (mut vcx, field) = fixture(cx, Arc::new(|_, _| Ok("attachment".into())));
    for (text, selection, expected) in [
        ("", 0..0, "attachment\nattachment\n"),
        ("before", 6..6, "before\nattachment\nattachment\n"),
        ("after", 0..0, "attachment\nattachment\nafter"),
        ("beforeafter", 6..6, "before\nattachment\nattachment\nafter"),
        (
            "before\nafter",
            6..6,
            "before\nattachment\nattachment\nafter",
        ),
        (
            "before\nafter",
            7..7,
            "before\nattachment\nattachment\nafter",
        ),
        (
            "before\n\nafter",
            7..7,
            "before\nattachment\nattachment\nafter",
        ),
        (
            "before\n\n\nafter",
            8..8,
            "before\n\nattachment\nattachment\nafter",
        ),
        (
            "beforeSELECTIONafter",
            6..15,
            "before\nattachment\nattachment\nafter",
        ),
    ] {
        vcx.update(|window, cx| {
            field.update(cx, |field, cx| {
                field.input.update(cx, |input, cx| {
                    input.set_value(text, window, cx);
                    input.set_selected_range(selection, cx);
                });
                start(
                    &field.uploads,
                    &field.input,
                    vec![Source::Image(gpui::Image::empty()); 2],
                    window,
                    cx,
                );
            });
        });
        vcx.run_until_parked();
        vcx.update(|_, cx| {
            let input = field.read(cx).input.read(cx);
            assert_eq!(input.value().as_str(), expected, "inserting into {text:?}");
            let cursor = expected.find("after").unwrap_or(expected.len());
            assert_eq!(input.selected_range(), cursor..cursor, "cursor in {text:?}");
        });
    }
}

#[gpui::test]
fn ordinary_text_paste_is_unchanged(cx: &mut TestAppContext) {
    let (mut vcx, field) = fixture(cx, Arc::new(|_, _| panic!("text must not upload")));
    vcx.update(|window, cx| {
        cx.write_to_clipboard(ClipboardItem::new_string("ordinary text".into()));
        gpui::test::TestWindowExt::press(window, "cmd-v", cx);
        assert_eq!(field.read(cx).input.read(cx).value(), "ordinary text");
    });
}

#[gpui::test]
fn file_drop_uploads_multiple_files_and_reports_partial_failure(cx: &mut TestAppContext) {
    let (mut vcx, field) = fixture(
        cx,
        Arc::new(|_, source| {
            if source.name() == "bad.png" {
                bail!("test upload failure");
            }
            Ok("https://github.com/user-attachments/assets/video".into())
        }),
    );
    let position = vcx.debug_bounds("comment-editor-field").unwrap().center();
    let paths =
        gpui::ExternalPaths(vec![PathBuf::from("clip.mp4"), PathBuf::from("bad.png")].into());
    vcx.update(|window, cx| {
        use gpui::InputEvent as _;
        window.dispatch_event(
            gpui::FileDropEvent::Entered {
                position,
                paths: paths.clone(),
            }
            .to_platform_input(),
            cx,
        );
        window.draw(cx).clear(cx);
        window.dispatch_event(
            gpui::FileDropEvent::Submit { position }.to_platform_input(),
            cx,
        );
        field.update(cx, |field, cx| {
            assert_eq!(field.uploads.read(cx).pending(), 2);
            field
                .input
                .update(cx, |input, cx| input.replace("later text", window, cx));
            assert!(blocks_submission(Some(&field.uploads), &field.input, cx));
        });
    });
    vcx.run_until_parked();
    vcx.update(|_, cx| {
        let field = field.read(cx);
        assert!(field
            .input
            .read(cx)
            .value()
            .contains("https://github.com/user-attachments/assets/video"));
        assert!(field.input.read(cx).value().contains("later text"));
        assert!(!field.input.read(cx).value().contains(MARKER_PREFIX));
        assert!(field
            .uploads
            .read(cx)
            .error
            .as_ref()
            .unwrap()
            .contains("test upload failure"));
        assert_eq!(field.uploads.read(cx).pending(), 0);
    });
}

#[gpui::test]
fn removed_placeholders_are_not_reinserted_by_late_uploads(cx: &mut TestAppContext) {
    let (mut vcx, field) = fixture(cx, Arc::new(|_, _| Ok("uploaded".into())));
    vcx.update(|window, cx| {
        field.update(cx, |field, cx| {
            start(
                &field.uploads,
                &field.input,
                vec![Source::Image(gpui::Image::empty())],
                window,
                cx,
            );
            field.input.update(cx, |input, cx| {
                input.set_value("replacement draft", window, cx)
            });
        });
    });
    vcx.run_until_parked();
    vcx.update(|_, cx| assert_eq!(field.read(cx).input.read(cx).value(), "replacement draft"));
}

#[gpui::test]
fn late_results_do_not_modify_a_replacement_composer(cx: &mut TestAppContext) {
    let (mut vcx, field) = fixture(cx, Arc::new(|_, _| Ok("old upload".into())));
    vcx.update(|window, cx| {
        field.update(cx, |field, cx| {
            start(
                &field.uploads,
                &field.input,
                vec![Source::Image(gpui::Image::empty())],
                window,
                cx,
            );
            field.input = cx.new(|cx| crate::comment_editor::new(window, cx));
            field
                .input
                .update(cx, |input, cx| input.set_value("new draft", window, cx));
            field.uploads = Uploads::new(
                github::PrLocator {
                    owner: "other".into(),
                    repo: "other".into(),
                    number: 2,
                },
                &field.input,
                cx,
            );
            cx.notify();
        });
    });
    vcx.run_until_parked();
    vcx.update(|_, cx| {
        let field = field.read(cx);
        assert_eq!(field.input.read(cx).value(), "new draft");
        assert_eq!(field.uploads.read(cx).pending(), 0);
        assert!(field.uploads.read(cx).error.is_none());
    });
}

#[gpui::test]
fn restored_placeholders_cannot_be_submitted(cx: &mut TestAppContext) {
    let (mut vcx, field) = fixture(cx, Arc::new(|_, _| Ok("uploaded".into())));
    vcx.update(|window, cx| {
        field.update(cx, |field, cx| {
            start(
                &field.uploads,
                &field.input,
                vec![Source::Image(gpui::Image::empty())],
                window,
                cx,
            );
        });
    });
    vcx.run_until_parked();
    vcx.simulate_keystrokes("cmd-z");
    vcx.update(|_, cx| {
        let field = field.read(cx);
        assert_eq!(field.uploads.read(cx).pending(), 0);
        assert!(field.input.read(cx).value().contains(MARKER_PREFIX));
        assert!(blocks_submission(Some(&field.uploads), &field.input, cx));
    });
    vcx.simulate_keystrokes("cmd-shift-z");
    vcx.update(|_, cx| {
        let field = field.read(cx);
        assert!(field.input.read(cx).value().contains("uploaded"));
        assert!(!blocks_submission(Some(&field.uploads), &field.input, cx));
    });
}

#[test]
fn invalid_files_fail_locally_before_authentication() {
    let loc = github::PrLocator {
        owner: "owner".into(),
        repo: "repo".into(),
        number: 1,
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("image.png");
    let file = std::fs::File::create(&path).unwrap();
    assert!(upload(&loc, Source::File(path.clone()))
        .unwrap_err()
        .to_string()
        .contains("empty"));
    file.set_len(IMAGE_LIMIT + 1).unwrap();
    assert!(upload(&loc, Source::File(path.clone()))
        .unwrap_err()
        .to_string()
        .contains("10 MB"));
    drop(file);
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(upload(&loc, Source::File(path))
        .unwrap_err()
        .to_string()
        .contains("regular file"));
    assert!(upload(&loc, Source::File("not-media.txt".into()))
        .unwrap_err()
        .to_string()
        .contains("Unsupported media"));
}

/// A local HTTP fixture exercises the actual raw-body transport without
/// authenticating or creating persistent assets on GitHub.
fn serve_once(
    status: &str,
    extra_headers: &str,
    body: &str,
) -> (Url, std::thread::JoinHandle<String>) {
    use std::io::Write as _;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = Url::parse(&format!("http://{}/upload", listener.local_addr().unwrap())).unwrap();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n{body}",
        body.len()
    );
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        loop {
            let mut chunk = [0; 4096];
            let len = stream.read(&mut chunk).unwrap();
            if len == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..len]);
            if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                let length: usize = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                if request.len() >= end + 4 + length {
                    break;
                }
            }
        }
        stream.write_all(response.as_bytes()).unwrap();
        String::from_utf8(request).unwrap()
    });
    (url, handle)
}

#[test]
fn transport_sends_raw_bytes_and_refuses_redirects_and_permission_errors() {
    let body = r#"{"url":"https://github.com/user-attachments/assets/123"}"#;
    let (url, request) = serve_once("201 Created", "", body);
    assert_eq!(
        send_upload(&url, "test-token", b"file bytes").unwrap(),
        body
    );
    let request = request.join().unwrap();
    let (headers, bytes) = request.split_once("\r\n\r\n").unwrap();
    assert_eq!(bytes, "file bytes");
    let headers = headers.to_ascii_lowercase();
    assert!(headers.contains("authorization: bearer test-token"));
    assert!(headers.contains("content-type: application/octet-stream"));
    assert!(headers.contains("accept: application/vnd.github+json"));
    let (url, request) = serve_once(
        "302 Found",
        "Location: http://127.0.0.1:1/must-not-follow\r\n",
        "",
    );
    assert!(send_upload(&url, "test-token", b"bytes")
        .unwrap_err()
        .to_string()
        .contains("unexpected upload response"));
    request.join().unwrap();
    let (url, request) = serve_once("404 Not Found", "", "{}");
    let error = send_upload(&url, "test-token", b"bytes")
        .unwrap_err()
        .to_string();
    assert!(error.contains("repository write access"));
    assert!(!error.contains("test-token"));
    request.join().unwrap();
}

#[test]
fn types_sizes_and_encoded_upload_query() {
    assert_eq!(content_type("SCREENSHOT.PNG").unwrap(), "image/png");
    assert_eq!(content_type("clip.mov").unwrap(), "video/quicktime");
    assert!(content_type("secret.txt").is_err());
    assert!(validate_size(0, "image/png").is_err());
    assert!(validate_size(IMAGE_LIMIT, "image/png").is_ok());
    assert!(validate_size(IMAGE_LIMIT + 1, "image/png").is_err());
    assert!(validate_size(VIDEO_LIMIT, "video/mp4").is_ok());
    assert!(validate_size(VIDEO_LIMIT + 1, "video/mp4").is_err());
    let url = upload_url("a & b#?.png", "image/png", 123);
    assert_eq!(url.host_str(), Some("uploads.github.com"));
    assert_eq!(
        url.query_pairs().collect::<Vec<_>>(),
        vec![
            ("name".into(), "a & b#?.png".into()),
            ("content_type".into(), "image/png".into()),
            ("repository_id".into(), "123".into()),
        ]
    );
}

#[test]
fn markdown_and_untrusted_responses() {
    let url = "https://github.com/user-attachments/assets/1234";
    let body = format!(r#"{{"url":"{url}"}}"#);
    assert_eq!(
        attachment_markdown("a[b].png", "image/png", &body).unwrap(),
        format!("![a\\[b\\].png]({url})")
    );
    assert_eq!(
        attachment_markdown("clip.mp4", "video/mp4", &body).unwrap(),
        url
    );
    for body in [
        "{}",
        "not json",
        r#"{"url":"https://example.com/asset"}"#,
        r#"{"url":"http://github.com/user-attachments/assets/a"}"#,
        r#"{"url":"https://github.com/user-attachments/assets/a)evil"}"#,
    ] {
        assert!(attachment_markdown("image.png", "image/png", body).is_err());
    }
}

#[test]
fn clipboard_prefers_original_files_and_leaves_text_alone() {
    assert!(clipboard_sources(&ClipboardItem::new_string("hello".into())).is_empty());
    assert!(matches!(
        clipboard_sources(&ClipboardItem::new_image(&gpui::Image::empty())).as_slice(),
        [Source::Image(_)]
    ));
    let item = ClipboardItem {
        entries: vec![
            ClipboardEntry::ExternalPaths(gpui::ExternalPaths(
                vec![PathBuf::from("a.png"), PathBuf::from("b.mp4")].into(),
            )),
            ClipboardEntry::Image(gpui::Image::empty()),
        ],
    };
    assert!(matches!(
        clipboard_sources(&item).as_slice(),
        [Source::File(_), Source::File(_)]
    ));
}
