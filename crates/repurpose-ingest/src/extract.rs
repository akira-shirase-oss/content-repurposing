//! The Readability pass: a decoded page in, the article's title and plain text out.
//!
//! The text returned here is the *source* every later span indexes into, so it is
//! normalised once, here, and never again: line endings are `\n`, controls other than
//! `\n` and `\t` are gone, trailing spaces are trimmed and blank runs collapse to one
//! empty line.

use dom_query::{Document, NodeRef};
use dom_smoothie::{Config, Readability, ReadabilityError, TextMode};
use url::Url;

use crate::error::ExtractError;

/// Fewer readable characters than this is not an article (a script-only shell, a
/// consent wall, an error page).
pub const MIN_ARTICLE_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Elements the Readability pass walks before refusing the page.
    pub max_elements: usize,
    /// Element nesting depth accepted. Readability's passes recurse per level, so an
    /// unbounded depth is a stack overflow that aborts the process.
    pub max_depth: usize,
    /// Bytes of article text kept; longer articles are cut at a paragraph or char
    /// boundary and marked truncated.
    pub max_text_bytes: usize,
    /// Characters of title kept.
    pub max_title_chars: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            // LIMIT(extract-elements)
            max_elements: 20_000,
            // LIMIT(extract-depth)
            max_depth: 256,
            // LIMIT(extract-text-bytes)
            max_text_bytes: 256 * 1024,
            max_title_chars: 300,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Article {
    pub title: String,
    pub text: String,
    pub truncated: bool,
}

/// Extracts the article from decoded HTML. CPU-bound: call it off the async workers.
pub fn extract_html(html: &str, url: &Url, limits: &Limits) -> Result<Article, ExtractError> {
    let config = Config {
        max_elements_to_parse: limits.max_elements,
        text_mode: TextMode::Formatted,
        ..Config::default()
    };
    prescan(html.as_bytes(), limits)?;
    let document = Document::from(html);
    check_shape(document.root(), limits)?;
    let mut readability = Readability::with_document(document, Some(url.as_str()), Some(config))
        .map_err(|_| ExtractError::NoArticle)?;
    let article = readability.parse().map_err(|err| match err {
        ReadabilityError::TooManyElements(..) => ExtractError::TooManyElements,
        _ => ExtractError::NoArticle,
    })?;
    finish(&article.title, &article.text_content, limits)
}

/// Elements with no content (never pushed) or whose end tag is optional and implied by a
/// sibling (pushing them would make an ordinary list look deep).
const NO_PUSH: &[&[u8]] = &[
    b"area",
    b"base",
    b"br",
    b"col",
    b"embed",
    b"hr",
    b"img",
    b"input",
    b"link",
    b"meta",
    b"source",
    b"track",
    b"wbr",
    b"p",
    b"li",
    b"dt",
    b"dd",
    b"option",
    b"tr",
    b"td",
    b"th",
    b"tbody",
    b"thead",
    b"tfoot",
    b"colgroup",
    b"rt",
    b"rp",
    b"optgroup",
];

/// Elements whose content is raw text, skipped to their end tag.
const RAW_TEXT: &[&[u8]] = &[
    b"script",
    b"style",
    b"textarea",
    b"title",
    b"xmp",
    b"noscript",
];

/// A byte-level estimate of tag count and nesting, run before the HTML parser: the
/// parser's cost grows with depth times tags (deeply nested input took seconds), so a
/// page that is obviously too deep or too big is refused without being parsed.
fn prescan(html: &[u8], limits: &Limits) -> Result<(), ExtractError> {
    let max_tags = limits.max_elements.saturating_mul(2);
    let mut stack: Vec<&[u8]> = Vec::new();
    let mut tags = 0usize;
    let mut i = 0;
    while let Some(at) = html
        .get(i..)
        .and_then(|rest| rest.iter().position(|&b| b == b'<'))
    {
        i += at + 1;
        let rest = html.get(i..).unwrap_or_default();
        if rest.starts_with(b"!--") {
            i += find(rest, b"-->").map_or(rest.len(), |e| e + 3);
            continue;
        }
        let (closing, rest) = match rest.strip_prefix(b"/") {
            Some(r) => (true, r),
            None => (false, rest),
        };
        let len = rest
            .iter()
            .take(16)
            .take_while(|b| b.is_ascii_alphanumeric())
            .count();
        if len == 0 || !rest[0].is_ascii_alphabetic() {
            continue;
        }
        let name = &rest[..len];
        let lower = name.to_ascii_lowercase();
        if closing {
            if let Some(pos) = stack
                .iter()
                .rposition(|open| open.eq_ignore_ascii_case(name))
            {
                stack.truncate(pos);
            }
            continue;
        }
        tags += 1;
        if tags > max_tags {
            return Err(ExtractError::TooManyElements);
        }
        if RAW_TEXT.contains(&lower.as_slice()) {
            let mut end = b"</".to_vec();
            end.extend_from_slice(&lower);
            let rest = html.get(i..).unwrap_or_default();
            i += rest
                .windows(end.len())
                .position(|w| w.eq_ignore_ascii_case(&end))
                .unwrap_or(rest.len());
            continue;
        }
        if NO_PUSH.contains(&lower.as_slice()) {
            continue;
        }
        stack.push(name);
        if stack.len() > limits.max_depth {
            return Err(ExtractError::TooDeep);
        }
    }
    Ok(())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Walks the tree without recursion and refuses it before anything recursive does:
/// more than `max_elements` elements, or deeper than `max_depth`.
fn check_shape(root: NodeRef<'_>, limits: &Limits) -> Result<(), ExtractError> {
    let mut elements = 0usize;
    let mut stack = vec![(root, 0usize)];
    while let Some((node, depth)) = stack.pop() {
        if depth > limits.max_depth {
            return Err(ExtractError::TooDeep);
        }
        if node.is_element() {
            elements += 1;
            if elements > limits.max_elements {
                return Err(ExtractError::TooManyElements);
            }
        }
        let mut child = node.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            stack.push((c, depth + 1));
        }
    }
    Ok(())
}

/// A `text/plain` page is its own article.
pub fn extract_plain(text: &str, limits: &Limits) -> Result<Article, ExtractError> {
    finish("", text, limits)
}

fn finish(title: &str, text: &str, limits: &Limits) -> Result<Article, ExtractError> {
    let text = normalize(text);
    if text.chars().filter(|c| !c.is_whitespace()).count() < MIN_ARTICLE_CHARS {
        return Err(ExtractError::NoArticle);
    }
    let (text, truncated) = cut(text, limits.max_text_bytes);
    let title: String = normalize(title)
        .replace('\n', " ")
        .chars()
        .take(limits.max_title_chars)
        .collect();
    Ok(Article {
        title,
        text,
        truncated,
    })
}

/// The one normalisation every source text goes through.
pub fn normalize(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut blank_run = 0usize;
    for line in raw.split('\n') {
        let line: String = line
            .chars()
            .filter(|&c| c == '\t' || !c.is_control())
            .collect();
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank_run += 1;
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
            if blank_run > 0 {
                out.push('\n');
            }
        }
        blank_run = 0;
        out.push_str(line);
    }
    out
}

/// Cuts `text` to at most `max` bytes, preferring the last paragraph break.
fn cut(mut text: String, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if let Some(para) = text[..end].rfind("\n\n")
        && para > end / 2
    {
        end = para;
    }
    text.truncate(end);
    let trimmed = text.trim_end().len();
    text.truncate(trimmed);
    (text, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url() -> Url {
        Url::parse("https://example.com/post").unwrap_or_else(|_| unreachable!())
    }

    fn paragraph(n: usize) -> String {
        format!(
            "<p>Paragraph {n} explains why bounded queues keep a service honest under load, \
             with a measured 42 percent drop in tail latency.</p>"
        )
    }

    #[test]
    fn the_default_limits_hold() {
        let many = format!(
            "<html><body>{}</body></html>",
            "<span>x</span>".repeat(20_001)
        );
        assert_eq!(
            extract_html(&many, &url(), &Limits::default()),
            Err(ExtractError::TooManyElements)
        );
        let deep = format!("<html><body>{}</body></html>", "<div>".repeat(300));
        assert_eq!(
            extract_html(&deep, &url(), &Limits::default()),
            Err(ExtractError::TooDeep)
        );
        let long: String = (0..3_000).map(paragraph).collect();
        let article = extract_html(
            &format!("<html><body><article>{long}</article></body></html>"),
            &url(),
            &Limits::default(),
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert!(article.truncated);
        assert!(article.text.len() <= 256 * 1024);
    }

    #[test]
    fn extracts_title_and_paragraphs() {
        let body: String = (0..6).map(paragraph).collect();
        let html = format!(
            "<html><head><title>Bounded queues</title></head><body><nav>Home | About</nav>\
             <article><h1>Bounded queues</h1>{body}</article><footer>(c)</footer></body></html>"
        );
        let article =
            extract_html(&html, &url(), &Limits::default()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(article.title, "Bounded queues");
        assert!(article.text.contains("42 percent"));
        assert!(!article.text.contains('\r'));
        assert!(!article.truncated);
    }

    #[test]
    fn a_script_only_page_has_no_article() {
        let html = "<html><head><script>window.app=1</script></head><body><div id=root></div>\
                    <script src=/bundle.js></script><noscript>Enable JavaScript</noscript></body></html>";
        assert_eq!(
            extract_html(html, &url(), &Limits::default()),
            Err(ExtractError::NoArticle)
        );
    }

    #[test]
    fn malformed_html_still_extracts_or_fails_by_name() {
        let body: String = (0..6).map(paragraph).collect();
        let html = format!("<html><body><div><p>unclosed<b><i>{body}</td></tr></table><<<>>></p");
        let article = extract_html(&html, &url(), &Limits::default());
        assert!(
            matches!(&article, Ok(a) if a.text.contains("Paragraph 3"))
                || article == Err(ExtractError::NoArticle)
        );
    }

    #[test]
    fn element_cap_refuses_the_page() {
        let body = "<div><span>x</span></div>".repeat(5_000);
        let limits = Limits {
            max_elements: 1_000,
            ..Limits::default()
        };
        assert_eq!(
            extract_html(
                &format!("<html><body>{body}</body></html>"),
                &url(),
                &limits
            ),
            Err(ExtractError::TooManyElements)
        );
    }

    #[test]
    fn deep_nesting_is_refused_before_readability_recurses() {
        // 20 000 nested divs overflowed the stack inside Readability and aborted the
        // process before the depth bound existed.
        for depth in [300usize, 19_000, 400_000] {
            let html = format!("{}{}", "<div>".repeat(depth), "<p>word </p>".repeat(100));
            assert_eq!(
                extract_html(&html, &url(), &Limits::default()),
                Err(ExtractError::TooDeep),
                "{depth}"
            );
        }
        let scripts = format!("{}{}", "<script>x</script>".repeat(30_000), paragraph(1));
        assert_eq!(
            extract_html(&scripts, &url(), &Limits::default()),
            Err(ExtractError::TooManyElements)
        );
        let shallow = format!("{}{}", "<div>".repeat(100), paragraph(1).repeat(4));
        assert!(extract_html(&shallow, &url(), &Limits::default()).is_ok());
        // Stray end tags do not hide depth from the DOM check behind the prescan.
        let stray = format!("{}{}", "<div></span>".repeat(300), paragraph(1));
        assert_eq!(
            extract_html(&stray, &url(), &Limits::default()),
            Err(ExtractError::TooDeep)
        );
        // A long flat list with optional end tags is not deep.
        let list = format!(
            "<ul>{}</ul>{}",
            "<li>item".repeat(5_000),
            paragraph(2).repeat(4)
        );
        assert!(extract_html(&list, &url(), &Limits::default()).is_ok());
    }

    #[test]
    fn long_text_is_cut_and_marked() {
        let text = "word ".repeat(100_000);
        let limits = Limits {
            max_text_bytes: 10_000,
            ..Limits::default()
        };
        let article = extract_plain(&text, &limits).unwrap_or_else(|e| panic!("{e}"));
        assert!(article.truncated);
        assert!(article.text.len() <= 10_000);
    }

    #[test]
    fn normalize_is_idempotent() {
        let raw = "a\r\n\r\n\r\n b  \n\u{0}c\t\n\n\n";
        let once = normalize(&raw.replace('\r', ""));
        assert_eq!(once, "a\n\n b\nc");
        assert_eq!(normalize(&once), once);
    }
}
