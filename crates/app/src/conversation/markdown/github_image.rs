//! Recognize the image-only HTML that GitHub's paste/drop UI inserts.
//! This is not an HTML renderer: containers and non-GitHub images stay with Kit.
use std::{collections::HashMap, ops::Range};

#[derive(Debug, PartialEq)]
pub(super) struct GitHubImage {
    pub url: String,
    pub alt: String,
    pub width: Option<f32>,
    pub height: Option<f32>,
}

fn decode_entities(value: &str) -> String {
    let mut decoded = String::new();
    let mut rest = value;
    while let Some(start) = rest.find('&') {
        decoded.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end) = rest.find(';') else { break };
        let entity = &rest[1..end];
        let replacement = if let Some(hex) = entity
            .strip_prefix("#x")
            .or_else(|| entity.strip_prefix("#X"))
        {
            u32::from_str_radix(hex, 16)
                .ok()
                .and_then(char::from_u32)
                .map(|c| c.to_string())
        } else if let Some(decimal) = entity.strip_prefix('#') {
            decimal
                .parse::<u32>()
                .ok()
                .and_then(char::from_u32)
                .map(|c| c.to_string())
        } else {
            markdown::decode_named(entity, true)
        };
        if let Some(replacement) = replacement {
            decoded.push_str(&replacement);
            rest = &rest[end + 1..];
        } else {
            decoded.push('&');
            rest = &rest[1..];
        }
    }
    decoded.push_str(rest);
    decoded
}

fn github_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    matches!(
        authority.to_ascii_lowercase().as_str(),
        "github.com"
            | "user-images.githubusercontent.com"
            | "private-user-images.githubusercontent.com"
    )
}

fn dimension(value: Option<&String>) -> Option<Option<f32>> {
    let Some(value) = value else {
        return Some(None);
    };
    // GitHub writes numeric pixel dimensions. Leave percentage/CSS sizing to
    // the stock renderer rather than silently changing its interpretation.
    let number = value
        .trim()
        .strip_suffix("px")
        .unwrap_or(value.trim())
        .parse::<f32>()
        .ok()?;
    (number.is_finite() && number > 0.).then_some(Some(number))
}

struct Fragment<'a> {
    images: Vec<(Range<usize>, GitHubImage)>,
    tail: &'a str,
}

fn prefix(source: &str) -> Option<Fragment<'_>> {
    let mut rest = source;
    let mut images = Vec::new();
    loop {
        rest = rest.trim_start();
        if rest.is_empty()
            || !rest
                .get(..4)
                .is_some_and(|tag| tag.eq_ignore_ascii_case("<img"))
        {
            return (!images.is_empty()).then_some(Fragment { images, tail: rest });
        }
        let offset = source.len() - rest.len();
        rest = &rest[4..];
        if !rest.starts_with(|c: char| c.is_ascii_whitespace() || c == '>' || c == '/') {
            return None;
        }
        let mut attrs = HashMap::new();
        loop {
            rest = rest.trim_start();
            if let Some(after) = rest.strip_prefix('>') {
                rest = after;
                break;
            }
            if let Some(after) = rest.strip_prefix("/>") {
                rest = after;
                break;
            }
            let key_end = rest
                .find(|c: char| c.is_ascii_whitespace() || matches!(c, '=' | '/' | '>'))
                .unwrap_or(rest.len());
            if key_end == 0 {
                return None;
            }
            let key = rest[..key_end].to_ascii_lowercase();
            rest = rest[key_end..].trim_start();
            let value = if let Some(after) = rest.strip_prefix('=') {
                rest = after.trim_start();
                match rest.as_bytes().first()? {
                    b'\'' | b'"' => {
                        let quote = rest.as_bytes()[0] as char;
                        rest = &rest[1..];
                        let end = rest.find(quote)?;
                        let value = &rest[..end];
                        rest = &rest[end + 1..];
                        value
                    }
                    _ => {
                        let end = rest
                            .find(|c: char| c.is_ascii_whitespace() || c == '>')
                            .unwrap_or(rest.len());
                        if end == 0 {
                            return None;
                        }
                        let value = &rest[..end];
                        rest = &rest[end..];
                        value
                    }
                }
            } else {
                ""
            };
            attrs.entry(key).or_insert_with(|| decode_entities(value));
        }
        let url = attrs.remove("src")?;
        if !github_url(&url) {
            return None;
        }
        images.push((
            offset..source.len() - rest.len(),
            GitHubImage {
                url,
                alt: attrs.remove("alt").unwrap_or_default(),
                width: dimension(attrs.get("width"))?,
                height: dimension(attrs.get("height"))?,
            },
        ));
    }
}

/// Parse image-only HTML. Offsets are relative to the fragment.
pub(super) fn parse(source: &str) -> Option<Vec<(usize, GitHubImage)>> {
    let fragment = prefix(source)?;
    fragment.tail.is_empty().then(|| {
        fragment
            .images
            .into_iter()
            .map(|(range, image)| (range.start, image))
            .collect()
    })
}

pub(super) struct CaptionedImage {
    pub html: String,
    pub images: Vec<GitHubImage>,
}

/// Without a blank line, CommonMark puts a pasted image and the following
/// caption in ONE opaque HTML node. Keep that node on Kit's stock HTML renderer,
/// adding links only around its leading GitHub image tags. Do not claim HTML
/// containers or reinterpret the caption as Markdown.
pub(super) fn captioned(source: &str) -> Option<CaptionedImage> {
    let fragment = prefix(source)?;
    if fragment.tail.is_empty() || fragment.tail.contains('<') {
        return None;
    }
    let mut html = String::new();
    let mut images = Vec::new();
    let mut cursor = 0;
    for (index, (range, image)) in fragment.images.into_iter().enumerate() {
        html.push_str(&source[cursor..range.start]);
        html.push_str(&format!("<a href=\"gander-image:{index}\">"));
        html.push_str(&source[range.clone()]);
        html.push_str("</a>");
        cursor = range.end;
        images.push(image);
    }
    html.push_str(&source[cursor..]);
    Some(CaptionedImage { html, images })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captions_stay_on_stock_html_and_only_image_tags_get_activation_links() {
        let tag = "<img width='120' height='80' alt='Image > detail' src='https://github.com/user-attachments/assets/a'>";
        let source = format!("{tag}\n{tag}\nCaption **is literal HTML text** &amp; details");
        let caption = captioned(&source).unwrap();
        assert_eq!(caption.images.len(), 2);
        assert_eq!(caption.html, format!("<a href=\"gander-image:0\">{tag}</a>\n<a href=\"gander-image:1\">{tag}</a>\nCaption **is literal HTML text** &amp; details"));
        assert!(captioned(&format!("{tag}\n<a href='https://example.com'>link</a>")).is_none());
        assert!(captioned(&format!("<div>{tag} caption</div>")).is_none());
    }

    #[test]
    fn github_paste_markup_preserves_dimensions_alt_and_extensionless_source() {
        let source = "<img width=\"1440\" height=\"900\" alt=\"Image &amp; detail &quot;A&quot; &#128512;\" src=\"https://github.com/user-attachments/assets/abc?x=1&amp;y=2\" />";
        assert_eq!(
            parse(source),
            Some(vec![(
                0,
                GitHubImage {
                    url: "https://github.com/user-attachments/assets/abc?x=1&y=2".into(),
                    alt: "Image & detail \"A\" 😀".into(),
                    width: Some(1440.),
                    height: Some(900.),
                }
            )])
        );
    }

    #[test]
    fn quoted_unquoted_multiline_and_adjacent_github_images() {
        let first =
            "<IMG src='https://user-images.githubusercontent.com/1/a.png' width='120' height=80>";
        let source = format!("{first}\n\n<img\n src=https://private-user-images.githubusercontent.com/2/b.png alt='two > one'>\n");
        let images = parse(&source).unwrap();
        assert_eq!(images.len(), 2);
        assert_eq!(images[1].0, first.len() + 2);
        assert_eq!(images[1].1.alt, "two > one");
    }

    #[test]
    fn general_html_other_sources_and_unsupported_dimensions_stay_stock() {
        for source in [
            "<div><img src='https://github.com/a'></div>",
            "<a href='https://example.com'><img src='https://github.com/a'></a>",
            "<img src='https://example.com/a'>",
            "<img src='https://github.com.evil.test/a'>",
            "<img src='https://github.com@evil.test/a'>",
            "<img src='file:///secret'>",
            "<img src='data:image/png;base64,AA=='>",
            "<img src='https://github.com/a' width='100%'>",
            "<img src='https://github.com/a' width='NaN'>",
            "<img src='https://github.com/a' width='-1'>",
            "<img src='https://github.com/a'",
            "<img src='https://github.com/a'> text",
            "<imgur src='https://github.com/a'>",
        ] {
            assert!(parse(source).is_none(), "{source}");
        }
    }
}
