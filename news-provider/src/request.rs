//! What a vendor is asked. A header value may be a credential's NAME and there is no variant
//! carrying its value, because `filter_params` is written into an archive nothing may delete:
//! `docs/design/architecture.md` section 3.2's `filter_params` row.

use canon::digest::{filter_params, Param as CanonParam, ParamValue};
use canon::{CanonError, Digest, FileDigest};
use chrono::NaiveDate;

use crate::provider::Provenance;

/// The four names a request and its ledger row are keyed by. One macro rather than four identical
/// impl blocks, and four types rather than `String`, because `canon::digest::attempt_id` takes
/// three of them positionally and three `String`s swap silently.
macro_rules! name_type {
    ($($(#[$doc:meta])* $type:ident),* $(,)?) => { $(
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub struct $type(String);

        impl $type {
            #[must_use]
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    )* };
}

name_type!(
    /// The adapter a request belongs to. `canon` takes a `&str` instead: it may not depend on the
    /// crate that owns this type, because cargo refuses the cycle.
    SourceId,
    /// What was asked for, as the ledger row names it.
    TargetWindow,
    /// One collection run.
    RunId,
    /// A credential by name. There is no type in this workspace that carries both a name and a
    /// value except `wire::Credentials`, which never leaves the process.
    CredentialRef,
);

name_type!(
    /// An HTTP header's name.
    HeaderName,
);

/// A header value, or a request parameter's: the same two shapes under the same rule, so one type
/// and not two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderValue {
    Literal(String),
    /// Substituted inside `Wire::send`, so the plan and the ledger hold the name.
    FromCredential(CredentialRef),
}

/// One parameter as sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    pub name: String,
    pub value: HeaderValue,
}

/// Which step of two-phase discovery a request is. A field rather than a url shape or a
/// per-vendor convention, because `Provider::extract` matches on it: a payload read under the
/// wrong parser returns zero rows, which is the empty fallback section 5 row 1 forbids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestPhase {
    /// A listing of the vendor's own rows, which the follow-ups come out of.
    Index,
    /// One document a listing named.
    Document,
}

impl RequestPhase {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Index => "index",
            Self::Document => "document",
        }
    }
}

/// The parameter name the roster digest travels under, so it is inside `filter_params` for every
/// request and no provider can forget to put it there.
const UNIVERSE_VERSION: &str = "universe_version";

/// One vendor request. It holds no client and no clock: `Wire` has the client, and the two
/// instants are read either side of the socket by the code that can know them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub source: SourceId,
    pub target_window: TargetWindow,
    pub phase: RequestPhase,
    /// Per REQUEST and not per adapter: the same EDGAR adapter reading today's index is
    /// `live_ingest` and reading a 2024 one is `vendor_archive`. Decided by `Provider::plan`,
    /// which is the only method holding the instant that tells them apart.
    pub provenance: Provenance,
    pub url: String,
    pub headers: Vec<(HeaderName, HeaderValue)>,
    pub params: Vec<Param>,
    /// sha256 of the roster bytes as read. On the REQUEST because the roster decides at
    /// collection time how many of a payload's rows enter the archive: architecture.md section 2's
    /// `universe_version` row, and section 9's "a derive-side input" amended there on 2026-11-13.
    pub universe_version: FileDigest,
}

impl Request {
    /// The digest of what the vendor is asked, in which a credential appears as its name and the
    /// roster digest is one parameter, so a run whose roster changed is a different `filter_params`
    /// even when every sent parameter is identical.
    pub fn filter_params(&self) -> Result<Digest, CanonError> {
        let roster = self.universe_version.hex().to_owned();
        let mut params: Vec<CanonParam> = self
            .params
            .iter()
            .map(|param| CanonParam {
                name: &param.name,
                value: match &param.value {
                    HeaderValue::Literal(value) => ParamValue::Literal(value),
                    HeaderValue::FromCredential(credential) => {
                        ParamValue::Credential(credential.as_str())
                    }
                },
            })
            .collect();
        params.push(CanonParam {
            name: UNIVERSE_VERSION,
            value: ParamValue::Literal(&roster),
        });
        filter_params(&params)
    }
}

/// The days one target window names, or `None` when the text is not a day or a `FROM..TO` range.
/// An inverted range is `Some` and empty, because "you asked for nothing" and "I cannot read what
/// you asked for" are different answers.
#[must_use]
pub fn days_of(window: &TargetWindow) -> Option<Vec<NaiveDate>> {
    const RANGE: &str = "..";
    let text = window.as_str();
    let (first, last) = match text.split_once(RANGE) {
        Some((from, to)) => (
            from.parse::<NaiveDate>().ok()?,
            to.parse::<NaiveDate>().ok()?,
        ),
        None => {
            let day = text.parse::<NaiveDate>().ok()?;
            (day, day)
        }
    };
    let mut days = Vec::new();
    let mut day = first;
    while day <= last {
        days.push(day);
        day = day.succ_opt()?;
    }
    Some(days)
}

#[cfg(test)]
mod tests {
    use super::{days_of, HeaderValue, Param, Request, RequestPhase, SourceId, TargetWindow};
    use crate::provider::Provenance;
    use canon::FileDigest;
    use chrono::NaiveDate;

    const ONE_DAY: &str = "2026-11-10";
    const A_RANGE: &str = "2026-11-09..2026-11-12";
    const INVERTED: &str = "2026-11-12..2026-11-09";
    const A_QUARTER: &str = "2026-QTR4";
    const DAYS_IN_THE_RANGE: usize = 4;
    const ROSTER: &[u8] = b"{\"320193\":\"AAPL\"}";
    const A_LATER_ROSTER: &[u8] = b"{\"320193\":\"AAPL\",\"789019\":\"MSFT\"}";

    fn day(text: &str) -> Option<NaiveDate> {
        text.parse().ok()
    }

    fn request(universe_version: &[u8]) -> Request {
        Request {
            source: SourceId::new("edgar_press"),
            target_window: TargetWindow::new(ONE_DAY),
            phase: RequestPhase::Index,
            provenance: Provenance::LiveIngest,
            url: "https://example.test/form.20261110.idx".to_owned(),
            headers: Vec::new(),
            params: vec![Param {
                name: "action".to_owned(),
                value: HeaderValue::Literal("getcompany".to_owned()),
            }],
            universe_version: FileDigest::of(universe_version),
        }
    }

    #[test]
    fn a_single_day_window_names_that_day_alone_and_a_range_names_every_day_in_it() {
        assert_eq!(
            days_of(&TargetWindow::new(ONE_DAY)),
            day(ONE_DAY).map(|only| vec![only])
        );
        assert_eq!(
            days_of(&TargetWindow::new(A_RANGE)).map(|days| days.len()),
            Some(DAYS_IN_THE_RANGE)
        );
    }

    /// The two answers a planner must tell apart: an inverted range is a window naming no day,
    /// and a quarter label is a window nothing can read. Collapsing them to one would make a
    /// mistyped window look like a closed market.
    #[test]
    fn an_inverted_range_names_no_day_while_an_unreadable_window_names_none_at_all() {
        assert_eq!(days_of(&TargetWindow::new(INVERTED)), Some(Vec::new()));
        assert_eq!(days_of(&TargetWindow::new(A_QUARTER)), None);
    }

    /// The daily index carries no ticker, so the filter is a roster lookup at collection time and
    /// an unrecorded roster change is drift nothing can see. Measured in the predecessor:
    /// `_load_cik_to_ticker` returns `{}` on a truncated file and the run reports a quiet day.
    #[test]
    fn two_runs_whose_rosters_differ_have_two_filter_params_digests() {
        assert_ne!(
            request(ROSTER).filter_params(),
            request(A_LATER_ROSTER).filter_params()
        );
    }
}
