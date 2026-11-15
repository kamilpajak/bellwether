//! A page's nesting depth, read from its source tags before html5ever parses it: html5ever's
//! cost grows with its stack of open elements (review 2026-11-15, release mode: 2,000 / 4,000 /
//! 8,000 nested `<div>` parsed in 0.009 / 0.024 / 0.068 s). The count is meant never to fall below
//! html5ever's own depth, less the `html` and `body` it implies when the source omits them;
//! architecture.md section 11.1 names what it does not model.

const VOID: [&str; 15] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "keygen", "link", "meta", "param",
    "source", "track", "wbr",
];
const RAW_TEXT: [&str; 8] = [
    "script", "style", "textarea", "title", "xmp", "iframe", "noembed", "noframes",
];
const PLAIN_TEXT: &str = "plaintext";
/// html5ever's "generate implied end tags" set.
const IMPLIED_END: [&str; 10] = [
    "dd", "dt", "li", "optgroup", "option", "p", "rb", "rp", "rt", "rtc",
];
/// Each closes an open element of its own name on top of the stack when it starts.
const CLOSES_ITS_SIBLING: [&str; 11] = [
    "dd", "dt", "li", "optgroup", "option", "p", "rb", "rp", "rt", "td", "tr",
];
const FOREIGN: [&str; 2] = ["svg", "math"];

/// The deepest element the source tags make, a void one included. An end tag closes only the
/// element on top, after any `IMPLIED_END` element above it, so sloppy markup counts deeper.
pub(crate) fn nesting_depth(page: &str) -> usize {
    let text = page.to_ascii_lowercase();
    // Each open element with the count of `FOREIGN` elements at or below it: O(1) per tag.
    let mut open: Vec<(&str, usize)> = Vec::new();
    let mut deepest = 0;
    let mut at = 0;
    while let Some(offset) = text.get(at..).and_then(|rest| rest.find('<')) {
        let start = at + offset;
        let rest = text.get(start..).unwrap_or("");
        at = if let Some(comment) = rest.strip_prefix("<!--") {
            start + "<!--".len() + comment_end(comment)
        } else if rest.starts_with("<!") || rest.starts_with("<?") {
            start + rest.find('>').map_or(rest.len(), |end| end + 1)
        } else if let Some(tail) = rest.strip_prefix("</") {
            if let Some(name) = tag_name(tail) {
                close(&mut open, name);
            }
            start + tag_end(rest)
        } else if let Some(name) = rest.get(1..).and_then(tag_name) {
            let end = start + tag_end(rest);
            let below = open.last().map_or(0, |(_, foreign)| *foreign);
            let foreign = below > 0;
            let self_closed = text.get(..end).is_some_and(|tag| tag.ends_with("/>"));
            if (foreign && self_closed) || (!foreign && VOID.contains(&name)) {
                deepest = deepest.max(open.len() + 1);
                end
            } else {
                if open.last().is_some_and(|(top, _)| *top == name)
                    && CLOSES_ITS_SIBLING.contains(&name)
                {
                    open.pop();
                }
                open.push((name, below + usize::from(FOREIGN.contains(&name))));
                deepest = deepest.max(open.len());
                match (foreign, name) {
                    (false, PLAIN_TEXT) => text.len(),
                    (false, raw) if RAW_TEXT.contains(&raw) => raw_text_end(&text, end, raw),
                    _ => end,
                }
            }
        } else {
            start + 1
        };
    }
    deepest
}

fn close(open: &mut Vec<(&str, usize)>, name: &str) {
    while open
        .last()
        .is_some_and(|(top, _)| *top != name && IMPLIED_END.contains(top))
    {
        open.pop();
    }
    if open.last().is_some_and(|(top, _)| *top == name) {
        open.pop();
    }
}

/// html5ever's tag name: an ASCII letter, then anything up to whitespace, `/` or `>`.
fn tag_name(tail: &str) -> Option<&str> {
    tail.starts_with(|first: char| first.is_ascii_alphabetic())
        .then(|| tail.split(|c: char| c.is_ascii_whitespace() || c == '/' || c == '>'))
        .and_then(|mut parts| parts.next())
}

/// The offset just past a tag's `>`, skipping a `>` inside a quoted attribute value.
fn tag_end(tag: &str) -> usize {
    let mut quote = None;
    let mut after_equals = false;
    for (index, byte) in tag.bytes().enumerate() {
        match (quote, byte) {
            (Some(open), _) if byte == open => quote = None,
            (Some(_), _) => {}
            (None, b'>') => return index + 1,
            (None, b'=') => after_equals = true,
            (None, b'"' | b'\'') if after_equals => {
                quote = Some(byte);
                after_equals = false;
            }
            (None, _) if after_equals && byte.is_ascii_whitespace() => {}
            (None, _) => after_equals = false,
        }
    }
    tag.len()
}

/// A comment ends at `-->` or `--!>`, or at once when it opens `<!-->` or `<!--->`.
fn comment_end(comment: &str) -> usize {
    if comment.starts_with('>') {
        return 1;
    }
    if comment.starts_with("->") {
        return 2;
    }
    let closes = [("-->", 3), ("--!>", 4)]
        .iter()
        .filter_map(|(close, length)| comment.find(close).map(|at| at + length))
        .min();
    closes.unwrap_or(comment.len())
}

/// Raw text runs to its own end tag, which the main loop then reads.
fn raw_text_end(text: &str, from: usize, name: &str) -> usize {
    text.get(from..)
        .and_then(|rest| rest.find(&format!("</{name}")))
        .map_or(text.len(), |offset| from + offset)
}

#[cfg(test)]
mod tests {
    use super::nesting_depth;

    /// Each row turns on one rule of the scan; the depth is html5ever's stack, or more.
    const ONE_RULE_EACH: [(&str, usize); 15] = [
        ("<div><div></div><div>", 2),
        ("<DIV><div></DIV><div>", 2),
        ("<div><br><br>", 2),
        ("<div/><div/>", 2),
        ("<ul><li><li><li>", 2),
        ("<div><p></div><div>", 2),
        ("<span><div></span><div>", 3),
        ("<!--><div><div>", 2),
        ("<!---><div>", 1),
        ("<!-- a --!><div>", 1),
        ("<div a=\">x<!--\"><div>", 2),
        ("<script><div></script><div>", 1),
        ("<svg><style><g><g>", 4),
        ("<svg><g/><g/>", 2),
        ("<plaintext><div>", 1),
    ];

    #[test]
    fn each_rule_of_the_scan_counts_its_own_row() {
        let wrong: Vec<(&str, usize, usize)> = ONE_RULE_EACH
            .into_iter()
            .map(|(page, depth)| (page, depth, nesting_depth(page)))
            .filter(|(_, expected, counted)| expected != counted)
            .collect();
        assert_eq!(wrong, Vec::new());
    }
}
