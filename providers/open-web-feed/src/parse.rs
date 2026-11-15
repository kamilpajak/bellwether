//! The two payload readers: RSS through an XML parser and pages through an HTML5 one. One HTML
//! parser for both was rejected: HTML treats `<link>` as a void element, so an RSS link's text is
//! lost.

use scraper::{ElementRef, Html};

use crate::rules::{Pick, Rule};

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
    if !root.has_tag_name("rss") {
        return None;
    }
    let channel = root.children().find(|node| node.has_tag_name("channel"))?;
    let child_text = |item: roxmltree::Node, name: &str| {
        item.children()
            .find(|node| node.has_tag_name(name))
            .and_then(|node| node.text())
            .map(|text| text.trim().to_owned())
    };
    Some(
        channel
            .children()
            .filter(|node| node.has_tag_name("item"))
            .map(|item| IndexRow {
                link: child_text(item, "link"),
                valid_raw: child_text(item, rule.valid_field.as_str()),
            })
            .collect(),
    )
}

/// What one article page yields under its rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Page {
    pub(crate) title: Option<String>,
    pub(crate) canonical: Option<String>,
    /// Whitespace-collapsed paragraph text, one line per non-empty paragraph; empty when the
    /// container is absent or holds no paragraph text.
    pub(crate) body: String,
}

pub(crate) fn page(payload: &[u8], rule: &Rule) -> Page {
    let html = Html::parse_document(&String::from_utf8_lossy(payload));
    let body = html
        .select(&rule.container)
        .next()
        .map(|container| {
            container
                .select(&rule.paragraph)
                .map(collapsed)
                .filter(|paragraph| !paragraph.is_empty())
                .collect::<Vec<String>>()
                .join("\n")
        })
        .unwrap_or_default();
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
    element
        .text()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ")
}
