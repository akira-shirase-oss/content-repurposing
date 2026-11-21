//! The Readability pass: a decoded page in, the article's title and plain text out.
//!
//! The text returned here is the *source* every later span indexes into, so it is
//! normalised once, here, and never again: line endings are `\n`, controls other than
//! `\n` and `\t` are gone, trailing spaces are trimmed and blank runs collapse to one
//! empty line.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

use dom_query::{Document, NodeId, NodeRef};
use dom_smoothie::{Config, Readability, ReadabilityError, TextMode};
use html5ever::interface::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tree_builder::TreeBuilderOpts;
use html5ever::{Attribute, ParseOpts, QualName, parse_document};
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
    extract_html_until(html, url, limits, &AtomicBool::new(false))
}

/// [`extract_html`], abandoned between input chunks once `cancel` is set.
pub fn extract_html_until(
    html: &str,
    url: &Url,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<Article, ExtractError> {
    let config = Config {
        max_elements_to_parse: limits.max_elements,
        text_mode: TextMode::Formatted,
        ..Config::default()
    };
    let (document, _) = parse(html, limits, cancel);
    let document = document?;
    check_shape(document.root(), limits)?;
    let mut readability = Readability::with_document(document, Some(url.as_str()), Some(config))
        .map_err(|_| ExtractError::NoArticle)?;
    let article = readability.parse().map_err(|err| match err {
        ReadabilityError::TooManyElements(..) => ExtractError::TooManyElements,
        _ => ExtractError::NoArticle,
    })?;
    finish(&article.title, &article.text_content, limits)
}

/// Input fed to the parser at a time; the bounds are checked between chunks.
const CHUNK: usize = 4 * 1024;

/// What a parse cost: input bytes fed to the parser and tree-building operations.
/// Both are bounded by the limits, whatever the page: a hostile page is refused within
/// a chunk of crossing a bound, not after the whole page is parsed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Work {
    pub bytes_fed: usize,
    pub sink_ops: u64,
}

/// Parses `html` as [`extract_html`] does and reports the work done; for the hostile
/// page suite's operation-count thresholds.
#[doc(hidden)]
pub fn parse_work(html: &str, limits: &Limits) -> (Result<(), ExtractError>, Work) {
    let (document, work) = parse(html, limits, &AtomicBool::new(false));
    (document.map(drop), work)
}

/// The tree builder's sink, counting as it builds: the element count and each new
/// node's depth are known when the node is attached, so a page too deep or too large is
/// refused as it crosses the bound. The parser's own costs grow with its open-element
/// stack, which the depth bound caps.
struct Budget {
    max_depth: usize,
    max_elements: usize,
    elements: Cell<usize>,
    ops: Cell<u64>,
    over: Cell<Option<ExtractError>>,
}

impl Budget {
    fn op(&self) {
        self.ops.set(self.ops.get().saturating_add(1));
    }

    fn refuse(&self, error: ExtractError) {
        if self.over.get().is_none() {
            self.over.set(Some(error));
        }
    }
}

struct Bounded {
    doc: Document,
    depth: RefCell<HashMap<NodeId, usize>>,
    budget: Rc<Budget>,
}

impl Bounded {
    fn depth_of(&self, node: NodeId) -> usize {
        self.depth.borrow().get(&node).copied().unwrap_or(0)
    }

    fn place(&self, child: &NodeOrText<NodeId>, depth: usize) {
        if let NodeOrText::AppendNode(node) = child {
            self.depth.borrow_mut().insert(*node, depth);
            if depth > self.budget.max_depth {
                self.budget.refuse(ExtractError::TooDeep);
            }
        }
    }
}

impl TreeSink for Bounded {
    type Handle = NodeId;
    type Output = Document;
    type ElemName<'a> = <Document as TreeSink>::ElemName<'a>;

    fn finish(self) -> Document {
        self.doc
    }

    // Parse errors are not kept: a hostile page can raise one per byte.
    fn parse_error(&self, _msg: Cow<'static, str>) {
        self.budget.op();
    }

    fn get_document(&self) -> NodeId {
        self.doc.get_document()
    }

    fn elem_name<'a>(&'a self, target: &'a NodeId) -> Self::ElemName<'a> {
        self.doc.elem_name(target)
    }

    fn create_element(&self, name: QualName, attrs: Vec<Attribute>, flags: ElementFlags) -> NodeId {
        self.budget.op();
        let elements = self.budget.elements.get().saturating_add(1);
        self.budget.elements.set(elements);
        if elements > self.budget.max_elements {
            self.budget.refuse(ExtractError::TooManyElements);
        }
        self.doc.create_element(name, attrs, flags)
    }

    fn create_comment(&self, text: StrTendril) -> NodeId {
        self.budget.op();
        self.doc.create_comment(text)
    }

    fn create_pi(&self, target: StrTendril, data: StrTendril) -> NodeId {
        self.budget.op();
        self.doc.create_pi(target, data)
    }

    fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
        self.budget.op();
        self.place(&child, self.depth_of(*parent).saturating_add(1));
        self.doc.append(parent, child);
    }

    fn append_based_on_parent_node(
        &self,
        element: &NodeId,
        prev_element: &NodeId,
        child: NodeOrText<NodeId>,
    ) {
        self.budget.op();
        let depth = self
            .depth_of(*element)
            .max(self.depth_of(*prev_element).saturating_add(1));
        self.place(&child, depth);
        self.doc
            .append_based_on_parent_node(element, prev_element, child);
    }

    fn append_doctype_to_document(
        &self,
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    ) {
        self.budget.op();
        self.doc
            .append_doctype_to_document(name, public_id, system_id);
    }

    fn get_template_contents(&self, target: &NodeId) -> NodeId {
        self.doc.get_template_contents(target)
    }

    fn same_node(&self, x: &NodeId, y: &NodeId) -> bool {
        self.doc.same_node(x, y)
    }

    fn set_quirks_mode(&self, mode: QuirksMode) {
        self.doc.set_quirks_mode(mode);
    }

    fn append_before_sibling(&self, sibling: &NodeId, new_node: NodeOrText<NodeId>) {
        self.budget.op();
        self.place(&new_node, self.depth_of(*sibling));
        self.doc.append_before_sibling(sibling, new_node);
    }

    fn add_attrs_if_missing(&self, target: &NodeId, attrs: Vec<Attribute>) {
        self.budget.op();
        self.doc.add_attrs_if_missing(target, attrs);
    }

    fn remove_from_parent(&self, target: &NodeId) {
        self.budget.op();
        self.doc.remove_from_parent(target);
    }

    // Moved children keep their recorded depth; `check_shape` measures the finished
    // tree exactly before anything recursive walks it.
    fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
        self.budget.op();
        self.doc.reparent_children(node, new_parent);
    }

    fn is_mathml_annotation_xml_integration_point(&self, handle: &NodeId) -> bool {
        self.doc.is_mathml_annotation_xml_integration_point(handle)
    }
}

/// Parses `html` in chunks through the counting sink; stops at the first chunk that
/// crosses a bound or after `cancel` is set.
fn parse(
    html: &str,
    limits: &Limits,
    cancel: &AtomicBool,
) -> (Result<Document, ExtractError>, Work) {
    let budget = Rc::new(Budget {
        max_depth: limits.max_depth,
        max_elements: limits.max_elements,
        elements: Cell::new(0),
        ops: Cell::new(0),
        over: Cell::new(None),
    });
    let sink = Bounded {
        doc: Document::default(),
        depth: RefCell::default(),
        budget: Rc::clone(&budget),
    };
    let opts = ParseOpts {
        tree_builder: TreeBuilderOpts {
            scripting_enabled: false,
            ..TreeBuilderOpts::default()
        },
        ..ParseOpts::default()
    };
    let mut parser = parse_document(sink, opts);
    let mut work = Work::default();
    let mut rest = html;
    while !rest.is_empty() {
        let mut end = CHUNK.min(rest.len());
        while !rest.is_char_boundary(end) {
            end += 1;
        }
        let (chunk, tail) = rest.split_at(end);
        rest = tail;
        parser.process(StrTendril::from_slice(chunk));
        work.bytes_fed += chunk.len();
        work.sink_ops = budget.ops.get();
        if let Some(error) = budget.over.get() {
            return (Err(error), work);
        }
        if cancel.load(Ordering::Relaxed) {
            return (Err(ExtractError::Cancelled), work);
        }
    }
    let document = parser.finish();
    work.sink_ops = budget.ops.get();
    match budget.over.get() {
        Some(error) => (Err(error), work),
        None => (Ok(document), work),
    }
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
