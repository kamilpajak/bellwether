//! A paged index (architecture.md section 11.2, PRN completeness row): page N+1 of a news sitemap
//! is asked only while page N holds no key of the previous poll, and a poll's pages are complete
//! only when every boundary it read and its last page share a key.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use pit::datedness::Minute;

use crate::hosts::PagedIndex;
use crate::parse::index_rows;
use crate::rules::Rule;
use crate::{item_key_under, minute_of};

/// The keys one earlier poll read across its pages, and the oldest minute they listed: what the
/// next poll anchors to (`Continuity::is_basis`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Basis {
    keys: BTreeSet<String>,
    oldest: Option<DateTime<Utc>>,
}

impl Basis {
    #[must_use]
    pub fn keys(&self) -> &BTreeSet<String> {
        &self.keys
    }
}

/// How a poll's pages join the poll before it; every arm but `Complete` leaves a stretch of the
/// index not looked at, which is never read as empty or failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Continuity {
    Complete,
    /// Two neighbouring pages shared no key: a later page from an older generation (section 11.6).
    BoundaryNoSharedKey,
    /// The last page read holds no key of the basis poll, and the walk ran out: no next page was
    /// due, or the one asked answered with no row.
    NoPreviousKey,
    /// No earlier poll is a basis, so nothing before this poll's own pages was looked at.
    FirstPoll,
    /// The last page read holds no key of the basis poll and a next page was due but not read:
    /// it failed, or robots disallow it. That stretch is still listed, so the poll is no basis.
    NextPageUnread,
}

impl Continuity {
    pub const ALL: [Self; 5] = [
        Self::Complete,
        Self::BoundaryNoSharedKey,
        Self::NoPreviousKey,
        Self::FirstPoll,
        Self::NextPageUnread,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::BoundaryNoSharedKey => "boundary_no_shared_key",
            Self::NoPreviousKey => "no_previous_key",
            Self::FirstPoll => "first_poll",
            Self::NextPageUnread => "next_page_unread",
        }
    }

    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|arm| arm.name() == name)
    }

    /// A poll whose own boundaries all shared a key and whose walk ran out: the next poll anchors
    /// to it. A broken boundary or an unread next page is not, so the next poll reads back across.
    #[must_use]
    pub const fn is_basis(self) -> bool {
        !matches!(self, Self::BoundaryNoSharedKey | Self::NextPageUnread)
    }
}

/// One page row: its item key, and its listed minute with the text it was read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PageRow {
    key: String,
    listed: Option<(Minute, String)>,
}

/// Every row of a page that names a link `item_key` reads, or `None` when the page is not the
/// rule's index shape.
pub(crate) fn page_rows(
    bytes: &[u8],
    rule: &Rule,
    rewrites: &BTreeMap<String, String>,
) -> Option<Vec<PageRow>> {
    Some(
        index_rows(bytes, rule)?
            .into_iter()
            .filter_map(|row| {
                let key = item_key_under(rewrites, row.link.as_deref()?)?;
                let listed = row.valid_raw.and_then(|raw| {
                    minute_of(&raw, &rule.valid_format).map(|minute| (minute, raw))
                });
                Some(PageRow { key, listed })
            })
            .collect(),
    )
}

/// The row listed at the oldest minute, the first of them when several share it.
fn oldest(rows: &[PageRow]) -> Option<&(Minute, String)> {
    rows.iter()
        .filter_map(|row| row.listed.as_ref())
        .min_by_key(|(minute, _)| minute.start())
}

/// Page `page + 1` and the listed text of `rows`' oldest row, which that page must reach past, or
/// `None`. Pages list newest first (§11.6), so a page older than the basis cannot lead to it.
pub(crate) fn next_page(
    page: u32,
    rows: &[PageRow],
    reached: Option<Minute>,
    basis: Option<&Basis>,
) -> Option<(u32, String)> {
    let basis = basis?;
    if rows.iter().any(|row| basis.keys.contains(&row.key)) {
        return None;
    }
    let (minute, raw) = oldest(rows)?;
    let before_basis = basis.oldest.is_some_and(|from| minute.start() < from);
    let moved_back = page == 1 || reached.is_some_and(|at| minute.start() <= at.start());
    (!before_basis && moved_back)
        .then(|| page.checked_add(1).map(|next| (next, raw.clone())))
        .flatten()
}

fn keys(rows: &[PageRow]) -> BTreeSet<&str> {
    rows.iter().map(|row| row.key.as_str()).collect()
}

/// `pages` in page order from page 1; `None` when there is none. `ran_out`: the page after the last
/// one read answered with no row.
pub(crate) fn continuity(
    pages: &[Vec<PageRow>],
    basis: Option<&Basis>,
    ran_out: bool,
) -> Option<Continuity> {
    let last = pages.last()?;
    let Some(basis) = basis else {
        return Some(Continuity::FirstPoll);
    };
    let broken = pages
        .windows(2)
        .any(|pair| keys(&pair[0]).is_disjoint(&keys(&pair[1])));
    Some(if broken {
        Continuity::BoundaryNoSharedKey
    } else if last.iter().any(|row| basis.keys.contains(&row.key)) {
        Continuity::Complete
    } else if !ran_out && next_page_due(pages, basis) {
        Continuity::NextPageUnread
    } else {
        Continuity::NoPreviousKey
    })
}

/// Whether `next_page` plans a page after the last of `pages`, as the feed asked it.
fn next_page_due(pages: &[Vec<PageRow>], basis: &Basis) -> bool {
    let Some((last, before)) = pages.split_last() else {
        return false;
    };
    let reached = before
        .last()
        .and_then(|rows| oldest(rows))
        .map(|(minute, _)| *minute);
    u32::try_from(pages.len())
        .ok()
        .and_then(|page| next_page(page, last, reached, Some(basis)))
        .is_some()
}

pub(crate) fn basis_of(pages: &[Vec<PageRow>]) -> Basis {
    let rows: Vec<&PageRow> = pages.iter().flatten().collect();
    Basis {
        keys: rows.iter().map(|row| row.key.clone()).collect(),
        oldest: rows
            .iter()
            .filter_map(|row| row.listed.as_ref())
            .map(|(minute, _)| minute.start())
            .min(),
    }
}

/// Every admitted paged index with the rule it is read under: what `newsvault collect` needs to
/// find a poll's pages in the store, its basis and its continuity.
#[derive(Debug, Clone)]
pub struct Paging {
    indexes: Vec<(PagedIndex, Rule)>,
    rewrites: BTreeMap<String, String>,
}

impl Paging {
    pub(crate) const fn new(
        indexes: Vec<(PagedIndex, Rule)>,
        rewrites: BTreeMap<String, String>,
    ) -> Self {
        Self { indexes, rewrites }
    }

    /// Each paged index's page-1 url, in hosts-file order.
    pub fn index_urls(&self) -> impl Iterator<Item = &str> {
        self.indexes
            .iter()
            .map(|(paged, _)| paged.index_url.as_str())
    }

    /// The page `url` names under the paged index whose page 1 is `index_url`.
    #[must_use]
    pub fn page_of(&self, index_url: &str, url: &str) -> Option<u32> {
        self.index(index_url)?.0.page_of(url)
    }

    fn index(&self, index_url: &str) -> Option<&(PagedIndex, Rule)> {
        self.indexes
            .iter()
            .find(|(paged, _)| paged.index_url == index_url)
    }

    fn rows_of(&self, index_url: &str, pages: &[Vec<u8>]) -> Option<Vec<Vec<PageRow>>> {
        let (_, rule) = self.index(index_url)?;
        pages
            .iter()
            .map(|bytes| page_rows(bytes, rule, &self.rewrites))
            .collect()
    }

    /// The basis a poll that read `pages` (page order) leaves; `None` when one does not parse.
    #[must_use]
    pub fn basis_of(&self, index_url: &str, pages: &[Vec<u8>]) -> Option<Basis> {
        Some(basis_of(&self.rows_of(index_url, pages)?))
    }

    /// How `pages` (page order from page 1) join `basis`; `None` when there is no page or one
    /// does not parse. `ran_out`: the page after the last one read answered with no row.
    #[must_use]
    pub fn continuity(
        &self,
        index_url: &str,
        pages: &[Vec<u8>],
        basis: Option<&Basis>,
        ran_out: bool,
    ) -> Option<Continuity> {
        continuity(&self.rows_of(index_url, pages)?, basis, ran_out)
    }
}
