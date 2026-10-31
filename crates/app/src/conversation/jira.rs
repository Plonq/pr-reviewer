//! Link Jira keys only in Markdown prose, leaving code and existing links alone.
use ::markdown::{mdast::Node, to_mdast, ParseOptions};
use std::ops::Range;

const PREFIXES: [&str; 4] = ["PYAPI-", "BOT-", "CS-", "HELP-"];
const BASE_URL: &str = "https://ucroo-platform.atlassian.net/browse/";

pub(super) fn link_references(source: String) -> String {
    // Most bodies need no Jira processing. Use the same parser as Kit so we
    // can distinguish prose from link labels, URLs, images, and code without
    // reserializing (and potentially changing) the author's Markdown.
    if !PREFIXES.iter().any(|prefix| source.contains(prefix)) {
        return source;
    }
    let Ok(tree) = to_mdast(&source, &ParseOptions::gfm()) else {
        return source;
    };
    let mut references = Vec::new();
    collect_references(&tree, &source, &mut references);
    let mut out = String::with_capacity(source.len());
    let mut copied = 0;
    for range in references {
        let key = &source[range.clone()];
        out.push_str(&source[copied..range.start]);
        out.push_str(&format!("[{key}]({BASE_URL}{key})"));
        copied = range.end;
    }
    out.push_str(&source[copied..]);
    out
}

fn collect_references(node: &Node, source: &str, references: &mut Vec<Range<usize>>) {
    match node {
        Node::Link(_) | Node::LinkReference(_) => return,
        Node::Text(text) => {
            if let Some(position) = &text.position {
                let start = position.start.offset;
                let end = position.end.offset;
                let prose = &source[start..end];
                for (offset, _) in prose.char_indices() {
                    if let Some(length) = reference_len(prose, offset) {
                        references.push(start + offset..start + offset + length);
                    }
                }
            }
        }
        _ => {}
    }
    if let Some(children) = node.children() {
        for child in children {
            collect_references(child, source, references);
        }
    }
}

fn reference_len(source: &str, at: usize) -> Option<usize> {
    let rest = &source[at..];
    let prefix = PREFIXES.iter().find(|prefix| rest.starts_with(**prefix))?;
    let digits = rest[prefix.len()..]
        .bytes()
        .take_while(u8::is_ascii_digit)
        .count();
    let length = prefix.len() + digits;
    let token_char = |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '/');
    (digits > 0
        && !source[..at].chars().next_back().is_some_and(token_char)
        && !rest[length..].chars().next().is_some_and(token_char))
    .then_some(length)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(key: &str) -> String {
        format!("[{key}]({BASE_URL}{key})")
    }

    #[test]
    fn all_projects_link_in_prose_with_punctuation_and_formatting() {
        let source = "🎉 Fix PYAPI-1234, (BOT-2); **CS-345** and _HELP-67890_.\n";
        assert_eq!(
            link_references(source.into()),
            format!(
                "🎉 Fix {}, ({}); **{}** and _{}_.\n",
                link("PYAPI-1234"),
                link("BOT-2"),
                link("CS-345"),
                link("HELP-67890")
            )
        );
    }

    #[test]
    fn only_complete_keys_in_known_uppercase_projects_link() {
        let source = "PYAPI-X BOT- CS- HELP-nope OTHER-123 pyapi-123 bot-2 cs-3 help-4 \
                      XPYAPI-1 PYAPI-12abc CS-12_foo HELP-12-extra _BOT-3 éCS-4 \
                      src/BOT-5 PYAPI-１２";
        assert_eq!(link_references(source.into()), source);
    }

    #[test]
    fn code_links_images_urls_and_html_are_untouched() {
        let source = "`PYAPI-1` and ``BOT-2 ` CS-3``\n\n\
                      ```text\nHELP-4\n```\n\n~~~\nCS-5\n~~~\n\n\
                      \x20   PYAPI-6\n\n\
                      > ```\n> BOT-7\n> ```\n\n\
                      [PYAPI-8](https://example.com/CS-9 \"HELP-10\")\n\n\
                      [BOT-11][ticket]\n\n[ticket]: https://example.com/CS-12\n\n\
                      ![HELP-13](https://example.com/PYAPI-14)\n\n\
                      <https://example.com/BOT-15> https://example.com/CS-16\n\n\
                      <!-- HELP-17 -->\n\n<img alt=\"PYAPI-18\" src=\"CS-19\">\n";
        assert_eq!(link_references(source.into()), source);
    }

    #[test]
    fn prose_in_lists_quotes_and_tables_links_without_changing_markdown() {
        let source = "- PYAPI-1\n\n> BOT-2\n\n| Ticket |\n| --- |\n| CS-3 |\n";
        assert_eq!(
            link_references(source.into()),
            format!(
                "- {}\n\n> {}\n\n| Ticket |\n| --- |\n| {} |\n",
                link("PYAPI-1"),
                link("BOT-2"),
                link("CS-3")
            )
        );
    }

    #[test]
    fn already_linked_references_are_not_linked_again() {
        let once = link_references("PYAPI-1 and HELP-2".into());
        assert_eq!(link_references(once.clone()), once);
    }
}
