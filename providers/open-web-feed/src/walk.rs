//! GNW's id walk (architecture.md section 11.2, GNW row): each poll asks every release id above
//! its basis through the highest id the PREVIOUS index poll listed (owner decision PG-D2,
//! 2026-11-17), so no id is asked before a higher one was listed a poll period earlier.

use std::collections::BTreeSet;
use std::ops::RangeInclusive;

use crate::paging::Continuity;
use crate::parse::index_rows;
use crate::rules::Rule;
use crate::Keys;

/// The placeholders of a hosts row's `id_walk.url`.
pub(crate) const DATE: &str = "{date}";
pub(crate) const ID: &str = "{id}";

/// One release as its url path names it; `date` is the path's `<y>/<m>/<d>` as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Release {
    pub(crate) id: u64,
    pub(crate) language: String,
    pub(crate) date: String,
}

/// `<prefix><y>/<m>/<d>/<id>/<n>/<language>/<slug>`, the language lowercased; `None` otherwise.
pub(crate) fn release_of(prefix: &str, path: &str) -> Option<Release> {
    let parts: Vec<&str> = path.strip_prefix(prefix)?.split('/').collect();
    let [year, month, day, id, number, language, slug] = parts.as_slice() else {
        return None;
    };
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    let dated = [(year, 4), (month, 2), (day, 2)]
        .iter()
        .all(|(text, width)| digits(text) && text.len() == *width);
    let named = !language.is_empty()
        && language
            .bytes()
            .all(|byte| byte.is_ascii_alphabetic() || byte == b'-');
    if !(dated && digits(id) && !id.starts_with('0') && digits(number) && named) {
        return None;
    }
    (!slug.is_empty()).then_some(())?;
    Some(Release {
        id: id.parse().ok()?,
        language: language.to_ascii_lowercase(),
        date: format!("{year}/{month}/{day}"),
    })
}

/// A hosts row's walk: a url on the row's host holding `DATE` and `ID` once each, and the language
/// its path names. One id can be listed in ten languages (3404412, probe10 12:00Z), so an id is
/// answered for the walk only in this language.
#[derive(Debug, Clone)]
pub(crate) struct IdWalk {
    pub(crate) url: String,
    pub(crate) language: String,
}

impl IdWalk {
    pub(crate) fn url_for(&self, date: &str, id: u64) -> String {
        self.url.replace(DATE, date).replace(ID, &id.to_string())
    }
}

/// The ids one poll walks, `after` excluded and `through` included, asked under `date`; `answered`
/// holds the ids the store already holds an answer for in the walk's language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkRange {
    after: u64,
    through: u64,
    date: String,
    answered: BTreeSet<u64>,
}

impl WalkRange {
    #[must_use]
    pub const fn after(&self) -> u64 {
        self.after
    }

    #[must_use]
    pub const fn through(&self) -> u64 {
        self.through
    }

    /// The `<y>/<m>/<d>` every walked url carries: the path date of the frontier's own row.
    #[must_use]
    pub fn date(&self) -> &str {
        &self.date
    }

    /// Every id the range names, empty when `through` is not above `after`.
    #[must_use]
    pub fn ids(&self) -> RangeInclusive<u64> {
        self.after.saturating_add(1)..=self.through
    }

    pub(crate) fn answered(&self) -> &BTreeSet<u64> {
        &self.answered
    }
}

/// What one poll walks (section 11.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalkPlan {
    /// No usable answer of the index before this poll, or no basis poll.
    FirstPoll,
    /// A previous answer and a basis, and one of them lists no release on the walked host, so
    /// the range is not known.
    RangeUnread,
    Range(WalkRange),
}

impl WalkPlan {
    #[must_use]
    pub const fn range(&self) -> Option<&WalkRange> {
        match self {
            Self::Range(range) => Some(range),
            Self::FirstPoll | Self::RangeUnread => None,
        }
    }
}

/// The basis poll as the driver read it from the store: the id it walked through (`None`: it
/// walked nothing, a first poll) and its own index answer.
#[derive(Debug, Clone, Copy)]
pub struct WalkBasis<'a> {
    pub walked_through: Option<u64>,
    pub index: &'a [u8],
}

#[derive(Debug, Clone)]
pub(crate) struct WalkedIndex {
    pub(crate) index_url: String,
    pub(crate) host: String,
    pub(crate) walk: IdWalk,
    pub(crate) rule: Rule,
}

/// Every admitted index with an id walk: what `newsvault collect` needs to hand each one its
/// range and to tell, after the run, which ids of it are answered.
#[derive(Debug, Clone)]
pub struct Walk {
    indexes: Vec<WalkedIndex>,
    keys: Keys,
}

impl Walk {
    pub(crate) const fn new(indexes: Vec<WalkedIndex>, keys: Keys) -> Self {
        Self { indexes, keys }
    }

    /// Each walked index's url, in hosts-file order.
    pub fn index_urls(&self) -> impl Iterator<Item = &str> {
        self.indexes.iter().map(|index| index.index_url.as_str())
    }

    fn index(&self, index_url: &str) -> Option<&WalkedIndex> {
        self.indexes
            .iter()
            .find(|index| index.index_url == index_url)
    }

    /// The releases an answer of the index at `index_url` lists, on its own host; `None` when the
    /// answer is not the index's shape.
    fn releases(&self, index_url: &str, bytes: &[u8]) -> Option<Vec<Release>> {
        let index = self.index(index_url)?;
        let rows = index_rows(bytes, &index.rule)?;
        Some(
            rows.iter()
                .filter_map(|row| self.release(index, row.link.as_deref()?))
                .collect(),
        )
    }

    fn release(&self, index: &WalkedIndex, url: &str) -> Option<Release> {
        let (host, path) = self.keys.rewritten(url)?;
        (host == index.host)
            .then(|| release_of(&index.rule.path_prefix, &path))
            .flatten()
    }

    /// What a poll walks: after the basis's own `walked_through`, or below the lowest id its index
    /// listed when it walked nothing, through the highest id `previous` lists.
    #[must_use]
    pub fn range(
        &self,
        index_url: &str,
        previous: Option<&[u8]>,
        basis: Option<WalkBasis>,
        answered: BTreeSet<u64>,
    ) -> WalkPlan {
        let (Some(previous), Some(basis)) = (previous, basis) else {
            return WalkPlan::FirstPoll;
        };
        let frontier = self
            .releases(index_url, previous)
            .and_then(|releases| releases.into_iter().max_by_key(|release| release.id));
        let after = basis.walked_through.or_else(|| {
            self.releases(index_url, basis.index)?
                .iter()
                .map(|release| release.id)
                .min()?
                .checked_sub(1)
        });
        match (frontier, after) {
            (Some(frontier), Some(after)) => WalkPlan::Range(WalkRange {
                after,
                through: frontier.id,
                date: frontier.date,
                answered,
            }),
            _ => WalkPlan::RangeUnread,
        }
    }

    /// The release id `url` names on the walked host of `index_url`, when in the walk's language.
    #[must_use]
    pub fn release_id(&self, index_url: &str, url: &str) -> Option<u64> {
        let index = self.index(index_url)?;
        self.release(index, url)
            .filter(|release| release.language == index.walk.language)
            .map(|release| release.id)
    }

    /// The id `url` walks under `range`: `url` is the walk's own url for that id.
    #[must_use]
    pub fn walked_id(&self, index_url: &str, range: &WalkRange, url: &str) -> Option<u64> {
        let index = self.index(index_url)?;
        let id = self.release(index, url)?.id;
        (range.ids().contains(&id) && index.walk.url_for(&range.date, id) == url).then_some(id)
    }

    /// The language a 3xx's `location` moves the release `url` names to: the same id on the same
    /// host in another language (owner decision PG-D4, 2026-11-17); `None` for any other target.
    #[must_use]
    pub fn moved_to(&self, index_url: &str, url: &str, location: &str) -> Option<String> {
        let index = self.index(index_url)?;
        moved_to(
            &self.keys,
            &index.host,
            &index.rule.path_prefix,
            url,
            location,
        )
    }

    /// `FirstPoll` and `RangeUnread` as planned; else `Complete` when every id of the range is in
    /// `answered`, and `IdUnread` with the ids that are not.
    #[must_use]
    pub fn continuity(plan: &WalkPlan, answered: &BTreeSet<u64>) -> (Continuity, Vec<u64>) {
        let range = match plan {
            WalkPlan::FirstPoll => return (Continuity::FirstPoll, Vec::new()),
            WalkPlan::RangeUnread => return (Continuity::RangeUnread, Vec::new()),
            WalkPlan::Range(range) => range,
        };
        let unread: Vec<u64> = range.ids().filter(|id| !answered.contains(id)).collect();
        let continuity = if unread.is_empty() {
            Continuity::Complete
        } else {
            Continuity::IdUnread
        };
        (continuity, unread)
    }
}

/// `moved_to` over a host and prefix: `location` root-relative or absolute, on `host` after the
/// rewrite, naming the id `url` names in another language.
pub(crate) fn moved_to(
    keys: &Keys,
    host: &str,
    prefix: &str,
    url: &str,
    location: &str,
) -> Option<String> {
    let (asked_host, asked_path) = keys.rewritten(url)?;
    let asked = release_of(prefix, &asked_path)?;
    let (target_host, target_path) = if location.starts_with('/') {
        let path = location.split(['?', '#']).next().unwrap_or(location);
        (asked_host.clone(), path.to_owned())
    } else {
        keys.rewritten(location)?
    };
    let target = release_of(prefix, &target_path)?;
    let same = asked_host == host && target_host == host && target.id == asked.id;
    (same && target.language != asked.language).then_some(target.language)
}
