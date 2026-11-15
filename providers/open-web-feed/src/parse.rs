//! The two payload readers: RSS through an XML parser and pages through an HTML5 one. One HTML
//! parser for both was rejected: HTML treats `<link>` as a void element, so an RSS link's text is
//! lost.

use news_provider::provider::UnusableReason;
use scraper::{ElementRef, Html, Node};

use crate::depth::nesting_depth;
use crate::rules::{Pick, Rule};

/// The deepest page `page` parses: 8 times 14, the untrimmed 3403075 page's depth by
/// `depth::nesting_depth` and by html5ever (2026-11-15, from the 2026-11-14 probe body, which is
/// not kept). The trimmed `article_3403075.html` measures 7.
pub(crate) const DEEPEST_PAGE: usize = 14 * 8;
/// Elements whose text is never prose.
const NOT_PROSE: [&str; 4] = ["script", "style", "noscript", "template"];

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
    /// Whitespace-collapsed paragraph text, one line per paragraph holding a letter or a digit.
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
            let paragraphs: Vec<String> = container
                .select(&rule.paragraph)
                .map(collapsed)
                .filter(|paragraph| paragraph.chars().any(char::is_alphanumeric))
                .collect();
            if paragraphs.is_empty() {
                Err(UnusableReason::NoArticleBody)
            } else {
                Ok(paragraphs.join("\n"))
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

fn collapsed(element: ElementRef) -> String {
    let mut text = String::new();
    prose(element, &mut text);
    text.split_whitespace().collect::<Vec<&str>>().join(" ")
}

fn prose(element: ElementRef, into: &mut String) {
    for child in element.children() {
        match child.value() {
            Node::Text(text) => into.push_str(text),
            Node::Element(inner) if NOT_PROSE.contains(&inner.name()) => {}
            Node::Element(_) => {
                if let Some(inner) = ElementRef::wrap(child) {
                    prose(inner, into);
                }
            }
            _ => {}
        }
    }
}
