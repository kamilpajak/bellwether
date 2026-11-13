//! The harness every provider's own test calls, so the seam's claims are checked once rather than
//! re-asserted per adapter. It returns the breaches it found, all of them: a count of failures
//! hides which failure (CONTRIBUTING.md, "What was measured").
//!
//! It is not a gate over the provider's own vendor behaviour - that is the adapter's test suite.
//! It checks the four claims the seam makes about ANY provider: a plan accounts for every day its
//! window names, it asks only for days both the calendar and the listing allow, extraction is pure
//! over (bytes, rules), and a payload's counts add up.

use std::collections::BTreeMap;

use pit::ObservationTime;

use crate::ledger::{AttemptEvent, LedgerError, Sink};
use crate::provider::{precision_of, Calendar, Listing, Provider, RawBlob};
use crate::request::{days_of, Request, TargetWindow};
use chrono::NaiveDate;

/// The days a window names, or none when it names none and when nothing can read it: this harness
/// asks about the days a plan covered and both of those are zero days to cover.
fn days_or_none(window: &TargetWindow) -> Vec<NaiveDate> {
    days_of(window).unwrap_or_default()
}

/// One claim this harness found broken, named so a reader knows which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breach {
    pub check: &'static str,
    pub detail: String,
}

pub const PLAN_ACCOUNTS_FOR_EVERY_DAY: &str = "plan_accounts_for_every_day";
pub const PLAN_ASKS_ONLY_FOR_A_LISTED_SESSION: &str = "plan_asks_only_for_a_listed_session";
pub const EXTRACT_IS_PURE: &str = "extract_is_pure";
pub const THE_COUNTS_ADD_UP: &str = "the_counts_add_up";
pub const NO_ITEM_IS_FINER_THAN_THE_CEILING: &str = "no_item_is_finer_than_the_ceiling";

/// What the harness needs to ask its questions: the inputs of one planned run, plus one stored
/// payload per shape the adapter's own fixtures carry.
#[derive(Debug)]
pub struct Subject<'a> {
    pub target: TargetWindow,
    pub calendar: &'a Calendar,
    pub listing: &'a Listing,
    pub at: ObservationTime,
    pub payloads: &'a [(Request, RawBlob)],
}

/// Every breach, or an empty vector.
pub fn breaches(provider: &dyn Provider, subject: &Subject) -> Vec<Breach> {
    let mut found = Vec::new();
    match provider.plan(
        &subject.target,
        subject.calendar,
        subject.listing,
        subject.at,
    ) {
        Ok(plan) => {
            found.extend(plan_accounts_for_every_day(&subject.target, &plan));
            found.extend(plan_asks_only_for_a_listed_session(
                subject.calendar,
                subject.listing,
                &plan,
            ));
        }
        Err(refusal) => found.push(Breach {
            check: PLAN_ACCOUNTS_FOR_EVERY_DAY,
            detail: format!("the plan was refused: {refusal}"),
        }),
    }
    for (request, raw) in subject.payloads {
        let once = provider.extract(request, raw);
        let twice = provider.extract(request, raw);
        if once != twice {
            found.push(Breach {
                check: EXTRACT_IS_PURE,
                detail: format!(
                    "two extractions of {} under phase {} disagree",
                    raw.digest,
                    request.phase.name()
                ),
            });
        }
        let accounted =
            u32::try_from(once.items.len() + once.follow_ups.len() + once.unusable.len())
                .unwrap_or(u32::MAX)
                .saturating_add(once.filtered_out);
        if accounted != once.vendor_rows {
            found.push(Breach {
                check: THE_COUNTS_ADD_UP,
                detail: format!(
                    "{} holds {} vendor rows and accounts for {accounted}",
                    raw.digest, once.vendor_rows
                ),
            });
        }
        let ceiling = provider.precision_ceiling();
        for item in &once.items {
            if precision_of(&item.valid) < ceiling {
                found.push(Breach {
                    check: NO_ITEM_IS_FINER_THAN_THE_CEILING,
                    detail: format!(
                        "{} carries {} against a ceiling of {}",
                        item.source_item_id,
                        precision_of(&item.valid).label(),
                        ceiling.label()
                    ),
                });
            }
        }
    }
    found
}

/// Every day the window names appears once, as a request or as a `NotPlanned` row. A whole-window
/// row (an unreadable or empty window) accounts for the window instead.
fn plan_accounts_for_every_day(target: &TargetWindow, plan: &crate::Plan) -> Vec<Breach> {
    if plan
        .not_planned
        .iter()
        .any(|(window, _)| window.as_str() == target.as_str())
    {
        return Vec::new();
    }
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for request in &plan.requests {
        *seen
            .entry(request.target_window.as_str().to_owned())
            .or_default() += 1;
    }
    for (window, _) in &plan.not_planned {
        *seen.entry(window.as_str().to_owned()).or_default() += 1;
    }
    days_or_none(target)
        .iter()
        .filter_map(
            |day| match seen.get(&day.to_string()).copied().unwrap_or(0) {
                1 => None,
                times => Some(Breach {
                    check: PLAN_ACCOUNTS_FOR_EVERY_DAY,
                    detail: format!("{day} appears {times} times in the plan"),
                }),
            },
        )
        .collect()
}

fn plan_asks_only_for_a_listed_session(
    calendar: &Calendar,
    listing: &Listing,
    plan: &crate::Plan,
) -> Vec<Breach> {
    plan.requests
        .iter()
        .flat_map(|request| days_or_none(&request.target_window))
        .filter(|day| !calendar.is_session(*day) || !listing.names(*day))
        .map(|day| Breach {
            check: PLAN_ASKS_ONLY_FOR_A_LISTED_SESSION,
            detail: format!("{day} is closed or unlisted and the plan asks for it"),
        })
        .collect()
}

/// Every event it was handed. Public because a provider's own test asserts that a startup refusal
/// wrote NO vendor event, and the only honest way to assert that is with a sink that would have
/// recorded one.
#[derive(Debug, Default)]
pub struct RecordingSink {
    pub written: Vec<AttemptEvent>,
}

impl Sink for RecordingSink {
    fn write(&mut self, event: &AttemptEvent) -> Result<(), LedgerError> {
        self.written.push(event.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use super::{
        breaches, Subject, EXTRACT_IS_PURE, NO_ITEM_IS_FINER_THAN_THE_CEILING,
        PLAN_ACCOUNTS_FOR_EVERY_DAY, PLAN_ASKS_ONLY_FOR_A_LISTED_SESSION, THE_COUNTS_ADD_UP,
    };
    use crate::budget::{BudgetScope, RequestBudget};
    use crate::provider::{
        Calendar, CollectedItem, Extracted, Listing, Plan, PlanRefusal, Provenance, Provider,
        RawBlob, ValidPrecision, ValidSource,
    };
    use crate::request::{HeaderValue, Request, RequestPhase};
    use crate::wire::{RawResponse, Timeouts, TransportError, TransportProfile};
    use crate::{SourceId, TargetWindow, Verdict};
    use canon::FileDigest;
    use chrono::{DateTime, NaiveDate, Utc};
    use pit::datedness::Published;
    use pit::{ObservationTime, ValidTime};

    const SOURCE: &str = "mock";
    /// Any roster bytes: this harness is about the seam's claims and not about a universe.
    const ROSTER: &[u8] = b"{}";
    const CALENDAR: &[u8] = br#"{"schema":"calendar/1","sessions":["2026-11-09","2026-11-10"]}"#;
    const WINDOW: &str = "2026-11-09..2026-11-10";
    const CLOSED_DAY: &str = "2026-11-07";
    const AT: &str = "2026-11-10T18:00:00Z";
    const PATIENCE: Duration = Duration::from_secs(1);
    const ONE_ROW: u32 = 1;

    /// Which claim this mock breaks. Every arm is a breach the harness must find, which is what
    /// makes the harness a gate rather than a function nobody has watched fail.
    #[derive(Debug, Clone, Copy)]
    enum Break {
        Nothing,
        DropADay,
        AskForAClosedDay,
        ExtractDifferentlyEachTime,
        MiscountVendorRows,
        ClaimAnInstantUnderADayCeiling,
    }

    struct Mock {
        how: Break,
        extractions: AtomicU32,
    }

    impl Mock {
        fn breaking(how: Break) -> Self {
            Self {
                how,
                extractions: AtomicU32::new(0),
            }
        }

        fn request(&self, window: &str) -> Request {
            Request {
                source: SourceId::new(SOURCE),
                target_window: TargetWindow::new(window),
                phase: RequestPhase::Index,
                provenance: Provenance::LiveIngest,
                url: "https://example.test/index".to_owned(),
                headers: Vec::new(),
                params: Vec::new(),
                universe_version: FileDigest::of(ROSTER),
            }
        }

        fn item(&self, valid: Published) -> CollectedItem {
            CollectedItem {
                source: SourceId::new(SOURCE),
                source_item_id: self.extractions.load(Ordering::Relaxed).to_string(),
                url: "https://example.test/doc".to_owned(),
                canonical_url: "/doc".to_owned(),
                title: "A document".to_owned(),
                valid,
                valid_raw: String::new(),
                valid_field: "none".to_owned(),
                valid_source: ValidSource::OurExtraction,
                provenance: Provenance::LiveIngest,
                body: None,
            }
        }
    }

    impl Provider for Mock {
        fn source(&self) -> SourceId {
            SourceId::new(SOURCE)
        }

        fn transport(&self) -> TransportProfile {
            TransportProfile {
                user_agent: HeaderValue::Literal(SOURCE.to_owned()),
                timeouts: Timeouts {
                    global: PATIENCE,
                    per_call: PATIENCE,
                    resolve: PATIENCE,
                    connect: PATIENCE,
                    send_request: PATIENCE,
                    await_100: PATIENCE,
                    send_body: PATIENCE,
                    recv_response: PATIENCE,
                    recv_body: PATIENCE,
                },
                body_limit: 1,
                budget: RequestBudget {
                    min_interval: PATIENCE,
                    scope: BudgetScope::PerIp,
                },
            }
        }

        fn provenance(&self, _window: &TargetWindow, _at: ObservationTime) -> Provenance {
            Provenance::LiveIngest
        }

        fn precision_ceiling(&self) -> ValidPrecision {
            match self.how {
                Break::ClaimAnInstantUnderADayCeiling => ValidPrecision::Day,
                _ => ValidPrecision::Instant,
            }
        }

        fn universe_version(&self) -> FileDigest {
            FileDigest::of(ROSTER)
        }

        fn plan(
            &self,
            target: &TargetWindow,
            _calendar: &Calendar,
            _listing: &Listing,
            _at: ObservationTime,
        ) -> Result<Plan, PlanRefusal> {
            let requests = match self.how {
                Break::DropADay => vec![self.request("2026-11-09")],
                Break::AskForAClosedDay => vec![
                    self.request("2026-11-09"),
                    self.request("2026-11-10"),
                    self.request(CLOSED_DAY),
                ],
                _ => vec![self.request("2026-11-09"), self.request("2026-11-10")],
            };
            let _ = target;
            Ok(Plan {
                requests,
                not_planned: Vec::new(),
            })
        }

        fn classify(
            &self,
            _request: &Request,
            _answer: &Result<RawResponse, TransportError>,
        ) -> Verdict {
            Verdict::Usable
        }

        fn extract(&self, _request: &Request, _raw: &RawBlob) -> Extracted {
            self.extractions.fetch_add(1, Ordering::Relaxed);
            // The breach is a provider claiming a FINER precision than it declares; a coarser one
            // is legitimate, which is the EDGAR day fallback.
            let valid = Published::At(ValidTime::new(DateTime::<Utc>::UNIX_EPOCH));
            let source_item_id = match self.how {
                Break::ExtractDifferentlyEachTime => self.item(valid).source_item_id,
                _ => "stable".to_owned(),
            };
            Extracted {
                items: vec![CollectedItem {
                    source_item_id,
                    ..self.item(valid)
                }],
                follow_ups: Vec::new(),
                unusable: Vec::new(),
                vendor_rows: match self.how {
                    Break::MiscountVendorRows => ONE_ROW + 1,
                    _ => ONE_ROW,
                },
                filtered_out: 0,
            }
        }

        fn extract_rules_version(&self) -> FileDigest {
            FileDigest::of(b"{}")
        }
    }

    fn found(how: Break) -> Vec<&'static str> {
        let Ok(calendar) = Calendar::read(CALENDAR) else {
            return vec!["a readable calendar"];
        };
        let days: Vec<NaiveDate> = ["2026-11-09", "2026-11-10"]
            .iter()
            .filter_map(|day| day.parse().ok())
            .collect();
        let listing = Listing::of_days(&days);
        let mock = Mock::breaking(how);
        let payloads = [(
            mock.request(WINDOW),
            RawBlob {
                digest: "d".to_owned(),
                bytes: Vec::new(),
            },
        )];
        let subject = Subject {
            target: TargetWindow::new(WINDOW),
            calendar: &calendar,
            listing: &listing,
            at: ObservationTime::new(AT.parse().unwrap_or(DateTime::<Utc>::UNIX_EPOCH)),
            payloads: &payloads,
        };
        breaches(&mock, &subject)
            .into_iter()
            .map(|breach| breach.check)
            .collect()
    }

    #[test]
    fn a_provider_that_breaks_nothing_yields_no_breach() {
        assert_eq!(found(Break::Nothing), Vec::<&str>::new());
    }

    /// One breach per claim, each raised by a mock that breaks exactly that claim: without these
    /// the harness is a function nobody has watched fail.
    #[test]
    fn each_claim_of_the_harness_is_raised_by_a_provider_that_breaks_only_that_claim() {
        let cases = [
            (Break::DropADay, PLAN_ACCOUNTS_FOR_EVERY_DAY),
            (Break::AskForAClosedDay, PLAN_ASKS_ONLY_FOR_A_LISTED_SESSION),
            (Break::ExtractDifferentlyEachTime, EXTRACT_IS_PURE),
            (Break::MiscountVendorRows, THE_COUNTS_ADD_UP),
            (
                Break::ClaimAnInstantUnderADayCeiling,
                NO_ITEM_IS_FINER_THAN_THE_CEILING,
            ),
        ];
        for (how, expected) in cases {
            assert!(found(how).contains(&expected), "{how:?} raised nothing");
        }
    }
}
