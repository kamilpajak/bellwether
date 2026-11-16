//! The two payload readers: RSS through an XML parser and pages through an HTML5 one. One HTML
//! parser for both was rejected: HTML treats `<link>` as a void element, so an RSS link's text is
//! lost.

use news_provider::provider::UnusableReason;
use scraper::{ElementRef, Html, Node, Selector};

use crate::depth::nesting_depth;
use crate::rules::{Pick, Rule};

/// The deepest page `page` parses: 8 times 14, the untrimmed 3403075 page's depth by
/// `depth::nesting_depth` and by html5ever (2026-11-15, from the 2026-11-14 probe body, which is
/// not kept). The trimmed `article_3403075.html` measures 7.
pub(crate) const DEEPEST_PAGE: usize = 14 * 8;
/// Elements whose text is never prose.
const NOT_PROSE: [&str; 4] = ["script", "style", "noscript", "template"];
/// Read as a space: PRN's `<b>About SIGNAL<br /></b>SIGNAL is` otherwise fused to `SIGNALSIGNAL`
/// (302939553, 2026-11-14 probe page).
const LINE_BREAK: &str = "br";
/// Elements whose edges are a space inside one line: a PRN contacts cell of two `<p>` was read as
/// `Ambre James-Brown+45 3079 9289` (2026-11-14 probe page, not kept).
const BLOCKS: [&str; 33] = [
    "address",
    "article",
    "aside",
    "blockquote",
    "caption",
    "dd",
    "div",
    "dl",
    "dt",
    "figcaption",
    "figure",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "li",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
];
const TABLE: &str = "table";
const ROW: &str = "tr";
const CAPTION: &str = "caption";
const CELLS: [&str; 2] = ["td", "th"];
const CELL_SEPARATOR: &str = " | ";
const LINK: &str = "a";
/// Cloudflare's obfuscated address: its text is the literal `[email protected]` (P1, section 11.1).
const CLOUDFLARE_EMAIL_CLASS: &str = "__cf_email__";
const CLOUDFLARE_EMAIL_PATH: &str = "/cdn-cgi/l/email-protection";
const MAILTO: &str = "mailto:";

/// One RSS `<item>`, its two fields as the payload spells them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexRow {
    pub(crate) link: Option<String>,
    pub(crate) valid_raw: Option<String>,
}

/// The rows of an RSS 2.0 feed, or `None` when the payload is not one at all, which is what makes
/// a 200 carrying something else a permanent failure rather than a quiet hour.
pub(crate) fn index_rows(payload: &[u8], rule: &Rule) -> Option<Vec<IndexRow>> {
    let text = std::str::from_utf8(payload).ok()?;
    let document = roxmltree::Document::parse(text).ok()?;
    let root = document.root_element();
    if !is_rss(root, "rss") {
        return None;
    }
    let channel = root.children().find(|node| is_rss(*node, "channel"))?;
    let child_text = |item: roxmltree::Node, name: &str| {
        item.children()
            .find(|node| is_rss(*node, name))
            .and_then(|node| node.text())
            .map(|text| text.trim().to_owned())
    };
    Some(
        channel
            .children()
            .filter(|node| is_rss(*node, "item"))
            .map(|item| IndexRow {
                link: child_text(item, "link"),
                valid_raw: child_text(item, rule.valid_field.as_str()),
            })
            .collect(),
    )
}

/// RSS 2.0 elements carry no namespace; `has_tag_name(&str)` compares the local name alone, so
/// it also matched `atom:link` and `dc:pubDate` (review 2026-11-15).
fn is_rss(node: roxmltree::Node, name: &str) -> bool {
    let tag = node.tag_name();
    node.is_element() && tag.namespace().is_none() && tag.name() == name
}

/// What one article page yields under its rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Page {
    pub(crate) title: Option<String>,
    pub(crate) canonical: Option<String>,
    /// Whitespace-collapsed text, one line per paragraph or table row holding a letter or a digit.
    pub(crate) body: Result<String, UnusableReason>,
}

pub(crate) fn page(payload: &[u8], rule: &Rule) -> Page {
    let unread = |reason| Page {
        title: None,
        canonical: None,
        body: Err(reason),
    };
    let Ok(text) = std::str::from_utf8(payload) else {
        return unread(UnusableReason::PageNotUtf8);
    };
    if nesting_depth(text) > DEEPEST_PAGE {
        return unread(UnusableReason::PageTooDeep);
    }
    let html = Html::parse_document(text);
    let mut containers = html.select(&rule.container);
    let body = match (containers.next(), containers.next()) {
        (None, _) => Err(UnusableReason::NoArticleBody),
        (Some(_), Some(_)) => Err(UnusableReason::ArticleContainerAmbiguous),
        (Some(container), None) => {
            let mut lines = Vec::new();
            blocks(container, &rule.paragraph, &mut lines);
            lines.retain(|line| line.chars().any(char::is_alphanumeric));
            if lines.is_empty() {
                Err(UnusableReason::NoArticleBody)
            } else {
                Ok(lines.join("\n"))
            }
        }
    };
    Page {
        title: picked(&html, &rule.title),
        canonical: picked(&html, &rule.canonical),
        body,
    }
}

fn picked(html: &Html, pick: &Pick) -> Option<String> {
    html.select(&pick.selector)
        .find_map(|element| element.value().attr(&pick.attribute))
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// The container's lines in document order: each outermost `paragraph` match is one line, and
/// each table row is one line, so a `p` inside a cell is read once and a table inside a matched
/// `li` splits that line around its rows.
fn blocks(element: ElementRef, paragraph: &Selector, into: &mut Vec<String>) {
    for inner in element.children().filter_map(ElementRef::wrap) {
        if skipped(inner) {
            continue;
        }
        if inner.value().name() == TABLE {
            rows(inner, into);
        } else if paragraph.matches(&inner) {
            let mut text = String::new();
            prose(inner, &mut text, Some(into));
            into.push(collapse(&text));
        } else {
            blocks(inner, paragraph, into);
        }
    }
}

/// One line per `tr` of this table, its non-empty cells joined by `CELL_SEPARATOR`, and one for
/// its `caption`; a table nested in a cell is read as that cell's text.
fn rows(element: ElementRef, into: &mut Vec<String>) {
    for inner in element.children().filter_map(ElementRef::wrap) {
        if skipped(inner) {
            continue;
        }
        if inner.value().name() == ROW {
            let cells: Vec<String> = inner
                .children()
                .filter_map(ElementRef::wrap)
                .filter(|cell| CELLS.contains(&cell.value().name()) && !skipped(*cell))
                .map(collapsed)
                .filter(|cell| !cell.is_empty())
                .collect();
            into.push(cells.join(CELL_SEPARATOR));
        } else if inner.value().name() == CAPTION {
            into.push(collapsed(inner));
        } else {
            rows(inner, into);
        }
    }
}

/// Never prose, a Cloudflare email placeholder, or a link to an address.
fn skipped(element: ElementRef) -> bool {
    let value = element.value();
    NOT_PROSE.contains(&value.name())
        || value.classes().any(|class| class == CLOUDFLARE_EMAIL_CLASS)
        || (value.name() == LINK && value.attr("href").is_some_and(names_an_address))
}

fn names_an_address(href: &str) -> bool {
    let href = href.trim_start();
    href.contains(CLOUDFLARE_EMAIL_PATH)
        || href
            .get(..MAILTO.len())
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case(MAILTO))
}

fn collapsed(element: ElementRef) -> String {
    let mut text = String::new();
    prose(element, &mut text, None);
    collapse(&text)
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<&str>>().join(" ")
}

/// With `tables`, a table ends the text so far as one line and is read by `rows`; without, as in
/// a cell, it is read as text.
fn prose(element: ElementRef, into: &mut String, mut tables: Option<&mut Vec<String>>) {
    for child in element.children() {
        match child.value() {
            Node::Text(text) => into.push_str(text),
            Node::Element(_) => {
                let Some(inner) = ElementRef::wrap(child) else {
                    continue;
                };
                let name = inner.value().name();
                if skipped(inner) {
                    continue;
                }
                if name == LINE_BREAK {
                    into.push(' ');
                } else if let (TABLE, Some(lines)) = (name, tables.as_deref_mut()) {
                    lines.push(collapse(into));
                    into.clear();
                    rows(inner, lines);
                } else if BLOCKS.contains(&name) {
                    into.push(' ');
                    prose(inner, into, tables.as_deref_mut());
                    into.push(' ');
                } else {
                    prose(inner, into, tables.as_deref_mut());
                }
            }
            _ => {}
        }
    }
}
