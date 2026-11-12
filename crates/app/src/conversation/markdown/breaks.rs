//! GitHub's issue/PR prose uses hard breaks, unlike Markdown files.
//!
//! Adapt only soft line endings in parsed paragraph text to CommonMark hard
//! breaks. Editing the original source (rather than serializing the AST) keeps
//! code, HTML, tables, reference definitions and all other syntax intact.
use ::markdown::{mdast::Node, ParseOptions};

pub(in crate::conversation) fn hard_breaks(source: String) -> String {
    if !source.contains(['\r', '\n']) {
        return source;
    }
    // Match Kit's constructs so math contents, like code, are left alone.
    let mut options = ParseOptions::gfm();
    options.constructs.math_text = true;
    options.constructs.math_flow = true;
    let Ok(root) = ::markdown::to_mdast(&source, &options) else {
        return source;
    };
    let mut breaks = Vec::new();
    collect(&root, &source, false, &mut breaks);
    if breaks.is_empty() {
        return source;
    }
    let mut out = String::with_capacity(source.len() + breaks.len());
    let mut copied = 0;
    for offset in breaks {
        out.push_str(&source[copied..offset]);
        out.push('\\');
        copied = offset;
    }
    out.push_str(&source[copied..]);
    out
}

fn collect(node: &Node, source: &str, paragraph: bool, breaks: &mut Vec<usize>) {
    // Image alt text isn't rendered prose, and headings use normal GFM rules.
    if matches!(
        node,
        Node::Image(_) | Node::ImageReference(_) | Node::Heading(_)
    ) {
        return;
    }
    if let Node::Text(text) = node {
        if paragraph {
            if let Some(position) = &text.position {
                let start = position.start.offset;
                let raw = &source[start..position.end.offset];
                for (offset, byte) in raw.bytes().enumerate() {
                    if byte == b'\r' || (byte == b'\n' && !raw[..offset].ends_with('\r')) {
                        breaks.push(start + offset);
                    }
                }
            }
        }
    }
    if let Some(children) = node.children() {
        for child in children {
            collect(
                child,
                source,
                paragraph || matches!(node, Node::Paragraph(_)),
                breaks,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_newlines_in_paragraphs_become_hard_breaks() {
        for (source, expected) in [
            ("line1\nline2\nline3", "line1\\\nline2\\\nline3"),
            ("é\r\n二\rthree", "é\\\r\n二\\\rthree"),
            ("**one\ntwo**\n_three_", "**one\\\ntwo**\\\n_three_"),
            (
                "[one\ntwo](https://example.com)",
                "[one\\\ntwo](https://example.com)",
            ),
            ("> one\n> two", "> one\\\n> two"),
            ("- one\n  continued\n- two", "- one\\\n  continued\n- two"),
            ("one \ntwo\n", "one \\\ntwo\n"),
        ] {
            let adapted = hard_breaks(source.into());
            assert_eq!(adapted, expected, "{source:?}");
            assert_eq!(hard_breaks(adapted.clone()), adapted, "idempotent");
        }
    }

    #[test]
    fn structural_and_literal_newlines_are_not_changed() {
        for source in [
            "one\n\ntwo\n",
            "one  \ntwo\\\nthree<br>four",
            "# heading\n\nsetext\nheading\n=======",
            "- one\n- two\n\n> quote\n\n---",
            "`one\ntwo`\n\n``one\ntwo``",
            "```rust\nlet a = 1;\nlet b = 2;\n```",
            "~~~\none\ntwo\n~~~",
            "    one\n    two\n",
            "> ```\n> one\n> two\n> ```",
            "$$\nx + y\n$$",
            "$one\ntwo$",
            "<details>\n<summary>More</summary>\none\ntwo\n</details>",
            "<!-- one\ntwo -->",
            "<pre>one\ntwo</pre>",
            "| one | two |\n| --- | --- |\n| a | b |",
            "![one\ntwo](https://example.com/image)",
            "[ref]: https://example.com\n  \"one\ntwo\"\n\n[ref]",
        ] {
            assert_eq!(hard_breaks(source.into()), source, "{source:?}");
        }
    }

    #[test]
    fn source_line_endings_not_decoded_entities_determine_breaks() {
        let source = "a&#10;b &amp; c\nd";
        assert_eq!(hard_breaks(source.into()), "a&#10;b &amp; c\\\nd");
    }
}
