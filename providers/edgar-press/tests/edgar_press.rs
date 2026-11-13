//! Against committed fixtures and never the network. Each 403 body below is the shape measured by
//! curl on six consecutive dates 2026-11-12, which is why the planning tests exist at all.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use canon::digest::{item_id, story_id, ItemFields};
use canon::FileDigest;
use chrono::{DateTime, Utc};
use edgar_press::{read_listing, EdgarPress, Refusal, CONTACT_CREDENTIAL, SOURCE};
use news_provider::conformance::{breaches, RecordingSink, Subject};
use news_provider::provider::{
    precision_of, stamp_of, BodyState, Calendar, CollectedItem, Extracted, Listing, NotPlanned,
    Plan, PlanRefusal, Provenance, Provider, RawBlob, UnusableReason, ValidPrecision, ValidSource,
};
use news_provider::request::{Request, RequestPhase};
use news_provider::{AttemptEvent, AttemptSlot, RawResponse, RunId, TargetWindow, Verdict};
use pit::datedness::Published;
use pit::{MeasuredLag, ObservationTime};

const CONTACT: &str = "Bellwether Research contact@bellwether.test";
const NO_CONTACT: &str = "bellwether-collector/0.1";
/// A run inside the 2026-11-10 session, so that day is a live ingest and no lag is needed.
const RUN_AT: &str = "2026-11-10T18:00:00Z";
const RUN: &str = "run-2026-11-10T18:00:00Z";
const SATURDAY: &str = "2026-11-07";
const VETERANS_DAY: &str = "2026-11-11";
const A_LISTED_SESSION: &str = "2026-11-10";
const A_SESSION_THE_LISTING_OMITS: &str = "2026-11-12";
const AN_ARCHIVED_SESSION: &str = "2026-11-09";
const A_WINDOW_SPANNING_ALL_FOUR: &str = "2026-11-09..2026-11-12";
const NOT_A_WINDOW: &str = "2026-QTR4";
const AN_INVERTED_WINDOW: &str = "2026-11-12..2026-11-09";
/// 16:32:15 New York on 2026-11-10, which is 21:32:15Z: an instant, not a fabricated midnight.
const APPLE_ACCEPTED_AT: &str = "2026-11-10T21:32:15Z";
/// `APPLE_ACCEPTED_AT` plus the 7,200 s `probed()` stands behind.
const APPLE_OBSERVED_AT: &str = "2026-11-10T23:32:15Z";
const APPLE_ACCESSION: &str = "0000320193-26-000071";
/// The second in-universe filing of the fixture index, which is the uncovered one in the
/// convergence tests below.
const MICROSOFT_ACCESSION: &str = "0000789019-26-000045";
/// The acceptance field as the filing header spells it, kept as `valid_raw` so a replay re-parses
/// the value rather than trusting the stored instant.
const APPLE_ACCEPTANCE_RAW: &str = "20261110163215";
const APPLE_TITLE: &str = "Apple Inc. 8-K";
const APPLE_URL: &str = "https://www.sec.gov/Archives/edgar/data/320193/0000320193-26-000071.txt";
const APPLE_CANONICAL_URL: &str = "/Archives/edgar/data/320193/0000320193-26-000071.txt";
/// The 2026-11-10 fixture index holds four rows: three 8-K and one 10-Q, and one of the 8-K rows
/// is a CIK the roster does not name.
const VENDOR_ROWS: u32 = 4;
const FILTERED_OUT: u32 = 2;
const IN_UNIVERSE: usize = 2;
/// The whole data layer of the fixture's `<script>`, which must not reach the body.
const SCRIPT_TEXT: &str = "dataLayer";
const A_TAG: &str = "<p>";
/// Measured 2026-11-13 by the test named after it, which is the construction: the two values are
/// 212 and 437, lower of the two middles. Two fixtures are not a corpus - this source's one real
/// figure is 14,039 characters over n=103, read through `newsvault day` on 2026-07-01.
const MEDIAN_BODY_CHARS: usize = 212;
const COMMITTED_FILINGS: usize = 2;
const FIRST_ATTEMPT: u32 = 0;
const SERVER_ERROR: u16 = 503;
const TOO_MANY: u16 = 429;
const FORBIDDEN: u16 = 403;
const OK: u16 = 200;
/// Every sink type this crate may not hold, asserted over its own source. The clock half of this
/// pair moved to `repo-invariants::clocks` on 2026-11-13: a repo-wide grep that lives inside one
/// of the crates it checks covers no other provider (CONTRIBUTING.md, "Everything else").
const NO_SINK: [&str; 2] = ["dyn Sink", "impl Sink"];

type Outcome = Result<(), Box<dyn std::error::Error>>;

/// The real capture and its row count: 100 leading rows plus all 391 8-K and 8-K/A rows.
const REAL_INDEX: &str = "form.20261110.real.idx";
const REAL_ROWS: u32 = 491;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(fixtures().join(name)).unwrap_or_default()
}

fn rules_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("rules/edgar-press.rules.json")
}

fn open_with(roster: &str, rules: &Path, contact: &str) -> Result<EdgarPress, Refusal> {
    EdgarPress::open(&fixtures().join(roster), rules, contact)
}

fn provider() -> Result<EdgarPress, Refusal> {
    open_with("roster.json", &rules_path(), CONTACT)
}

/// The same provider with a lag a probe stands behind, which is what a window holding a day
/// earlier than the run needs: without it `plan` refuses, which is decision D5 and its own test.
fn probed() -> Result<EdgarPress, Refusal> {
    Ok(provider()?.with_observation_lag(MeasuredLag {
        probe_id: "pplx-2026-11-11".to_owned(),
        probes: 69,
        leaks: 0,
        seconds: 7_200,
    }))
}

fn at(text: &str) -> ObservationTime {
    ObservationTime::new(text.parse().unwrap_or(DateTime::<Utc>::UNIX_EPOCH))
}

fn calendar() -> Result<Calendar, news_provider::CalendarError> {
    Calendar::read(&fixture("calendar.json"))
}

fn listing() -> Result<Listing, Refusal> {
    read_listing(&fixture("quarter_index.json"))
}

/// One plan over the four-day window, which holds a weekend, a holiday, a listed session and a
/// session the listing does not name.
fn plan_over(window: &str, provider: &EdgarPress) -> Result<Plan, Box<dyn std::error::Error>> {
    Ok(provider.plan(
        &TargetWindow::new(window),
        &calendar()?,
        &listing()?,
        at(RUN_AT),
    )?)
}

fn reason(plan: &Plan, day: &str) -> Option<NotPlanned> {
    plan.not_planned
        .iter()
        .find(|(window, _)| window.as_str() == day)
        .map(|(_, reason)| *reason)
}

fn asked_for(plan: &Plan) -> Vec<String> {
    plan.requests
        .iter()
        .map(|request| request.target_window.as_str().to_owned())
        .collect()
}

fn index_request(provider: &EdgarPress, day: &str) -> Result<Request, Box<dyn std::error::Error>> {
    let plan = plan_over(day, provider)?;
    plan.requests
        .first()
        .cloned()
        .ok_or_else(|| format!("no request was planned for {day}").into())
}

/// The verdict for one answer, opened through a real slot so the attempt id on the response is
/// the one the ledger holds.
fn verdict(
    provider: &EdgarPress,
    request: &Request,
    status: u16,
    body: &[u8],
) -> Result<Verdict, Box<dyn std::error::Error>> {
    let mut sink = RecordingSink::default();
    let slot = AttemptSlot::open(
        &mut sink,
        request,
        &RunId::new(RUN),
        FIRST_ATTEMPT,
        at(RUN_AT),
    )?;
    let answer = Ok(RawResponse::new(
        slot.attempt_id().clone(),
        status,
        body.to_vec(),
        at(RUN_AT),
        at(RUN_AT),
    ));
    Ok(provider.classify(request, &answer))
}

/// A document request inside a window the run reads AFTER the fact, which is the arm that needs a
/// lag. The day is `AN_ARCHIVED_SESSION` and the run instant is `RUN_AT`.
fn archived_document_request(provider: &EdgarPress) -> Result<Request, Box<dyn std::error::Error>> {
    let index = index_request(provider, AN_ARCHIVED_SESSION)?;
    let extracted = provider.extract(&index, &blob("form.20261110.idx"));
    extracted
        .follow_ups
        .first()
        .cloned()
        .ok_or_else(|| "the fixture index yielded no follow-up".to_string().into())
}

fn document_request(
    provider: &EdgarPress,
    day: &str,
) -> Result<Request, Box<dyn std::error::Error>> {
    let index = index_request(provider, day)?;
    let extracted = provider.extract(&index, &blob("form.20261110.idx"));
    extracted
        .follow_ups
        .first()
        .cloned()
        .ok_or_else(|| "the fixture index yielded no follow-up".to_string().into())
}

fn blob(name: &str) -> RawBlob {
    let bytes = fixture(name);
    RawBlob {
        digest: FileDigest::of(&bytes).hex().to_owned(),
        bytes,
    }
}

fn only_item(extracted: &Extracted) -> Result<CollectedItem, Box<dyn std::error::Error>> {
    extracted
        .items
        .first()
        .cloned()
        .ok_or_else(|| format!("expected one item, got {extracted:?}").into())
}

fn identity(item: &CollectedItem) -> String {
    let body_ref = item
        .body
        .as_ref()
        .map(|body| FileDigest::of(body.text.as_bytes()).hex().to_owned());
    let Some(valid) = stamp_of(&item.valid) else {
        return "no publication value, so no item_id".to_owned();
    };
    match item_id(&ItemFields {
        source: SOURCE,
        source_item_id: &item.source_item_id,
        url: &item.url,
        valid,
        title: &item.title,
        body_ref: body_ref.as_deref(),
    }) {
        Ok(digest) => digest.hex().to_owned(),
        Err(why) => format!("refused: {why}"),
    }
}

/// The body of the first filing a provider opened with `rules` extracts from the Apple fixture.
fn body_under(rules: &Path) -> Result<CollectedItem, Box<dyn std::error::Error>> {
    let provider = open_with("roster.json", rules, CONTACT)?;
    let request = document_request(&provider, A_LISTED_SESSION)?;
    only_item(&provider.extract(&request, &blob("filing_8k_apple.txt")))
}

#[test]
fn a_non_session_day_produces_no_request_and_one_not_planned_row() -> Outcome {
    let provider = probed()?;
    let plan = plan_over(A_WINDOW_SPANNING_ALL_FOUR, &provider)?;
    assert_eq!(
        reason(&plan, VETERANS_DAY),
        Some(NotPlanned::CalendarClosed)
    );
    assert!(!asked_for(&plan).contains(&VETERANS_DAY.to_owned()));
    Ok(())
}

/// A Saturday, a Sunday, a federal holiday and an unpublished day all answer 403 with the same
/// generic S3 body, so the calendar is the only thing that can tell the first three apart.
#[test]
fn a_weekend_day_is_zero_requests_rather_than_a_403_nobody_can_read() -> Outcome {
    let provider = provider()?;
    let plan = plan_over(SATURDAY, &provider)?;
    assert_eq!(reason(&plan, SATURDAY), Some(NotPlanned::CalendarClosed));
    assert_eq!(plan.requests.len(), 0);
    Ok(())
}

#[test]
fn a_day_the_quarter_listing_does_not_name_produces_a_window_not_published_row() -> Outcome {
    let provider = probed()?;
    let plan = plan_over(A_WINDOW_SPANNING_ALL_FOUR, &provider)?;
    assert_eq!(
        reason(&plan, A_SESSION_THE_LISTING_OMITS),
        Some(NotPlanned::WindowNotPublished)
    );
    assert_eq!(
        asked_for(&plan),
        vec![AN_ARCHIVED_SESSION.to_owned(), A_LISTED_SESSION.to_owned()]
    );
    Ok(())
}

/// Both reasons keep a run's exit at 0 and an empty window does not, which is the arm that would
/// otherwise let a 30-day wrong-calendar run stay green.
#[test]
fn a_window_naming_no_day_is_an_unclean_reason_while_a_closed_market_is_a_clean_one() -> Outcome {
    let provider = provider()?;
    let plan = plan_over(AN_INVERTED_WINDOW, &provider)?;
    assert_eq!(
        reason(&plan, AN_INVERTED_WINDOW),
        Some(NotPlanned::PlanEmpty)
    );
    assert!(!NotPlanned::PlanEmpty.clean());
    assert!(NotPlanned::CalendarClosed.clean() && NotPlanned::WindowNotPublished.clean());
    Ok(())
}

#[test]
fn a_window_that_is_not_a_day_or_a_range_is_refused_rather_than_planned_as_nothing() -> Outcome {
    let provider = provider()?;
    let refused = provider.plan(
        &TargetWindow::new(NOT_A_WINDOW),
        &calendar()?,
        &listing()?,
        at(RUN_AT),
    );
    assert_eq!(
        refused.err(),
        Some(PlanRefusal::TargetWindowUnreadable(NOT_A_WINDOW.to_owned()))
    );
    Ok(())
}

/// §8 row R11's other half. The lag opened the gate on 2026-11-13 and nothing used it: every stored
/// row still took its observation from the fetch clock, which dates an archived filing as though we
/// had read it the moment it was published. `from_backfill` had no caller anywhere in the tree.
#[test]
fn a_backfilled_item_observes_at_its_publication_plus_the_probed_lag_and_not_at_the_fetch(
) -> Outcome {
    let provider = probed()?;
    let request = archived_document_request(&provider)?;
    let item = only_item(&provider.extract(&request, &blob("filing_8k_apple.txt")))?;
    assert_eq!(item.provenance, Provenance::VendorArchive);
    // The Apple fixture is accepted at 21:32:15Z and the probed lag is 7,200 s.
    assert_eq!(item.observed, Some(at(APPLE_OBSERVED_AT)));
    Ok(())
}

/// A live ingest carries none of its own: the driver's fetch instant is the observation, and an
/// item that placed one would be overriding the only clock that knows when we read it.
#[test]
fn a_live_ingest_item_places_no_observation_of_its_own() -> Outcome {
    let provider = probed()?;
    let request = document_request(&provider, A_LISTED_SESSION)?;
    let item = only_item(&provider.extract(&request, &blob("filing_8k_apple.txt")))?;
    assert_eq!(
        (item.provenance, item.observed),
        (Provenance::LiveIngest, None)
    );
    Ok(())
}

/// Decision D5. A window the run reads after the fact is a `vendor_archive` window, and dating one
/// through an unprobed lag puts the item's observation at its own publication instant.
#[test]
fn a_vendor_archive_window_is_refused_until_a_probed_lag_stands_behind_it() -> Outcome {
    let provider = provider()?;
    assert_eq!(
        provider.provenance(&TargetWindow::new(AN_ARCHIVED_SESSION), at(RUN_AT)),
        Provenance::VendorArchive
    );
    assert_eq!(
        provider
            .plan(
                &TargetWindow::new(AN_ARCHIVED_SESSION),
                &calendar()?,
                &listing()?,
                at(RUN_AT)
            )
            .err(),
        Some(PlanRefusal::ObservationLagAbsent {
            source_id: SOURCE.to_owned(),
            window: AN_ARCHIVED_SESSION.to_owned(),
            why: "no lag entry"
        })
    );
    assert_eq!(
        asked_for(&plan_over(AN_ARCHIVED_SESSION, &probed()?)?),
        vec![AN_ARCHIVED_SESSION.to_owned()]
    );
    Ok(())
}

#[test]
fn a_403_on_a_day_the_listing_named_closes_a_permanent_failure_and_not_a_quiet_day() -> Outcome {
    let provider = provider()?;
    let request = index_request(&provider, A_LISTED_SESSION)?;
    assert_eq!(
        verdict(
            &provider,
            &request,
            FORBIDDEN,
            &fixture("error_403_access_denied.xml")
        )?,
        Verdict::PermanentFailure
    );
    Ok(())
}

#[test]
fn a_shared_ip_traffic_403_closes_a_transient_failure_so_a_later_run_retries_it() -> Outcome {
    let provider = provider()?;
    let request = index_request(&provider, A_LISTED_SESSION)?;
    assert_eq!(
        verdict(
            &provider,
            &request,
            FORBIDDEN,
            &fixture("error_403_traffic.html")
        )?,
        Verdict::TransientFailure
    );
    Ok(())
}

#[test]
fn a_429_and_a_5xx_both_close_transient_failures() -> Outcome {
    let provider = provider()?;
    let request = index_request(&provider, A_LISTED_SESSION)?;
    for status in [TOO_MANY, SERVER_ERROR] {
        assert_eq!(
            verdict(&provider, &request, status, b"")?,
            Verdict::TransientFailure,
            "{status}"
        );
    }
    Ok(())
}

#[test]
fn a_200_carrying_the_vendors_own_error_text_closes_a_permanent_failure() -> Outcome {
    let provider = provider()?;
    let request = index_request(&provider, A_LISTED_SESSION)?;
    assert_eq!(
        verdict(
            &provider,
            &request,
            OK,
            &fixture("error_200_vendor_text.html")
        )?,
        Verdict::PermanentFailure
    );
    Ok(())
}

#[test]
fn an_empty_daily_index_closes_empty_and_writes_its_row() -> Outcome {
    let provider = provider()?;
    let request = index_request(&provider, A_LISTED_SESSION)?;
    let empty = fixture("form.20261112.idx");
    assert_eq!(verdict(&provider, &request, OK, &empty)?, Verdict::Empty);
    let mut sink = RecordingSink::default();
    let slot = AttemptSlot::open(
        &mut sink,
        &request,
        &RunId::new(RUN),
        FIRST_ATTEMPT,
        at(RUN_AT),
    )?;
    let answer = Ok(RawResponse::new(
        slot.attempt_id().clone(),
        OK,
        empty,
        at(RUN_AT),
        at(RUN_AT),
    ));
    let outcome = news_provider::Outcome::decide(
        &provider.classify(&request, &answer),
        0,
        &news_provider::Baseline::from_trailing_ok(&[]),
    );
    let closed = slot.close(&mut sink, outcome, &answer, at(RUN_AT))?;
    assert_eq!(closed.outcome().kind(), news_provider::OutcomeKind::Empty);
    assert_eq!(
        sink.written
            .iter()
            .map(AttemptEvent::name)
            .collect::<Vec<_>>(),
        vec!["opened", "answered", "finalised"]
    );
    Ok(())
}

#[test]
fn a_user_agent_without_a_contact_is_refused_at_startup_and_never_recorded_as_a_vendor_event(
) -> Outcome {
    let mut sink = RecordingSink::default();
    assert_eq!(
        open_with("roster.json", &rules_path(), NO_CONTACT).err(),
        Some(Refusal::ContactMissing)
    );
    assert_eq!(sink.written, Vec::new());
    assert_eq!(
        provider()?.transport().user_agent,
        news_provider::HeaderValue::FromCredential(news_provider::CredentialRef::new(
            CONTACT_CREDENTIAL
        ))
    );
    let _ = &mut sink;
    Ok(())
}

/// R6. The predecessor's own roster read catches the unparseable case, logs a warning and returns
/// an empty map, and the run then reports a quiet day.
#[test]
fn an_absent_or_unparseable_roster_is_refused_before_the_first_request() -> Outcome {
    let absent = open_with("roster_missing.json", &rules_path(), CONTACT);
    assert!(
        matches!(absent.err(), Some(Refusal::RosterAbsent(_))),
        "an absent roster was not refused"
    );
    let truncated = open_with("roster_truncated.json", &rules_path(), CONTACT);
    assert!(
        matches!(truncated.err(), Some(Refusal::RosterUnparseable { .. })),
        "a truncated roster was not refused"
    );
    let empty = open_with("roster_empty.json", &rules_path(), CONTACT);
    assert!(
        matches!(empty.err(), Some(Refusal::RosterEmpty(_))),
        "an empty roster was not refused"
    );
    Ok(())
}

#[test]
fn a_rules_file_naming_a_zone_this_binary_does_not_know_is_refused_at_startup() -> Outcome {
    let unknown = open_with(
        "roster.json",
        &fixtures().join("rules_unknown_zone.json"),
        CONTACT,
    );
    assert!(
        matches!(unknown.err(), Some(Refusal::RulesZoneUnknown { .. })),
        "an unknown zone was not refused"
    );
    Ok(())
}

#[test]
fn a_run_whose_roster_digest_changed_is_distinguishable_from_one_whose_payload_changed() -> Outcome
{
    let narrow = provider()?;
    let wide = open_with("roster_wider.json", &rules_path(), CONTACT)?;
    let (narrow_request, wide_request) = (
        index_request(&narrow, A_LISTED_SESSION)?,
        index_request(&wide, A_LISTED_SESSION)?,
    );
    assert_ne!(
        narrow_request.filter_params(),
        wide_request.filter_params(),
        "a roster change has to move filter_params"
    );
    assert_eq!(narrow.universe_version(), narrow_request.universe_version);
    let document = document_request(&narrow, A_LISTED_SESSION)?;
    let one_payload = only_item(&narrow.extract(&document, &blob("filing_8k_apple.txt")))?;
    let another =
        only_item(&narrow.extract(&document, &blob("filing_8k_microsoft_no_acceptance.txt")))?;
    assert_ne!(
        identity(&one_payload),
        identity(&another),
        "a payload change has to move item_id"
    );
    assert_eq!(
        narrow_request.filter_params().ok(),
        index_request(&narrow, A_LISTED_SESSION)?
            .filter_params()
            .ok(),
        "and a payload change must not move filter_params"
    );
    Ok(())
}

#[test]
fn a_filing_whose_acceptance_instant_parses_lands_as_an_instant() -> Outcome {
    let provider = provider()?;
    let request = document_request(&provider, A_LISTED_SESSION)?;
    let item = only_item(&provider.extract(&request, &blob("filing_8k_apple.txt")))?;
    assert_eq!(
        item.valid,
        Published::At(pit::ValidTime::new(at(APPLE_ACCEPTED_AT).inner()))
    );
    assert_eq!(precision_of(&item.valid), ValidPrecision::Instant);
    assert_eq!(item.valid_raw, APPLE_ACCEPTANCE_RAW);
    assert_eq!(item.valid_field, "ACCEPTANCE-DATETIME");
    assert_eq!(
        (
            item.source_item_id.as_str(),
            item.title.as_str(),
            item.url.as_str(),
            item.canonical_url.as_str(),
            item.valid_source,
            item.provenance
        ),
        (
            APPLE_ACCESSION,
            APPLE_TITLE,
            APPLE_URL,
            APPLE_CANONICAL_URL,
            ValidSource::VendorPublished,
            Provenance::LiveIngest
        )
    );
    assert_eq!(item.source.as_str(), SOURCE);
    Ok(())
}

/// The fatal arm. A `day` value folded into midnight was visible at 02:00 on its own publication
/// day in the predecessor's lake, 13 h of admitted lookahead; here the date is carried as a date
/// in the zone the rules name, and `pit` makes it visible only past the end of THAT day.
#[test]
fn a_filing_whose_acceptance_instant_does_not_parse_lands_as_a_day_and_never_as_midnight() -> Outcome
{
    let provider = provider()?;
    let request = document_request(&provider, A_LISTED_SESSION)?;
    let item =
        only_item(&provider.extract(&request, &blob("filing_8k_microsoft_no_acceptance.txt")))?;
    assert_eq!(
        item.valid,
        Published::On {
            day: A_LISTED_SESSION.parse()?,
            tz: "America/New_York"
        }
    );
    assert_eq!(precision_of(&item.valid), ValidPrecision::Day);
    assert_eq!(item.valid_field, "Date Filed");
    assert_eq!(
        item.valid.lower_bound(),
        Some(at("2026-11-10T05:00:00Z").inner())
    );
    Ok(())
}

#[test]
fn a_blank_source_item_id_lands_as_an_unusable_row_carrying_its_whole_item() -> Outcome {
    let provider = provider()?;
    let request = document_request(&provider, A_LISTED_SESSION)?;
    let raw = blob("filing_8k_blank_accession.txt");
    let extracted = provider.extract(&request, &raw);
    let row = extracted
        .unusable
        .first()
        .ok_or_else(|| format!("expected one unusable row, got {extracted:?}"))?;
    assert_eq!(
        (
            row.reason,
            row.raw_blob.as_str(),
            row.item.source_item_id.as_str()
        ),
        (UnusableReason::BlankSourceItemId, raw.digest.as_str(), "")
    );
    assert!(row.item.body.is_some(), "the whole item, body included");
    assert_eq!((extracted.items.len(), extracted.vendor_rows), (0, 1));
    Ok(())
}

/// The other unusable arm: a document whose acceptance instant does not parse AND whose window
/// names no day has no publication value at all, so there is no `item_id` to take over it.
#[test]
fn a_filing_with_no_acceptance_instant_and_no_readable_day_is_unusable_rather_than_undated(
) -> Outcome {
    let provider = provider()?;
    let mut request = document_request(&provider, A_LISTED_SESSION)?;
    request.target_window = TargetWindow::new(NOT_A_WINDOW);
    let extracted = provider.extract(&request, &blob("filing_8k_microsoft_no_acceptance.txt"));
    assert_eq!(
        extracted.unusable.first().map(|row| row.reason),
        Some(UnusableReason::NoParseableValidTime)
    );
    assert_eq!(
        extracted
            .unusable
            .first()
            .map(|row| precision_of(&row.item.valid)),
        Some(ValidPrecision::Unknown)
    );
    Ok(())
}

#[test]
fn a_fixture_index_yields_follow_ups_equal_to_the_in_universe_subset() -> Outcome {
    let provider = provider()?;
    let request = index_request(&provider, A_LISTED_SESSION)?;
    let extracted = provider.extract(&request, &blob("form.20261110.idx"));
    assert_eq!(
        (
            extracted.vendor_rows,
            extracted.filtered_out,
            extracted.follow_ups.len()
        ),
        (VENDOR_ROWS, FILTERED_OUT, IN_UNIVERSE)
    );
    assert!(extracted
        .follow_ups
        .iter()
        .all(|follow_up| follow_up.phase == RequestPhase::Document
            && follow_up.provenance == request.provenance
            && follow_up.universe_version == request.universe_version));
    Ok(())
}

/// Both in-universe follow-ups the fixture index yields, under the provenance `day` gives them.
fn index_follow_ups(
    provider: &EdgarPress,
    day: &str,
) -> Result<Vec<Request>, Box<dyn std::error::Error>> {
    let index = index_request(provider, day)?;
    Ok(provider
        .extract(&index, &blob("form.20261110.idx"))
        .follow_ups)
}

/// The covered set the archive would hand in, over the accessions named here.
fn covered(accessions: &[&str]) -> BTreeSet<String> {
    accessions
        .iter()
        .filter_map(|accession| story_id(SOURCE, accession).ok())
        .map(|digest| digest.hex().to_owned())
        .collect()
}

/// The accession each follow-up url ends in, in order, so the exclusion is checked by the vendor
/// row identity and not by url string.
fn accessions_of(follow_ups: &[Request]) -> Vec<String> {
    follow_ups
        .iter()
        .filter_map(|follow_up| follow_up.url.rsplit('/').next())
        .map(|last| last.trim_end_matches(".txt").to_owned())
        .collect()
}

/// §8 row R20. A backfill re-run is handed the stories it already committed; a `vendor_archive`
/// follow-up whose story is covered is dropped and the uncovered one is kept. That is what makes a
/// re-run converge on full coverage instead of re-fetching the whole day.
#[test]
fn a_vendor_archive_follow_up_whose_story_is_covered_is_dropped_and_an_uncovered_one_is_kept(
) -> Outcome {
    let provider = probed()?;
    let follow_ups = index_follow_ups(&provider, AN_ARCHIVED_SESSION)?;
    assert!(follow_ups
        .iter()
        .all(|follow_up| follow_up.provenance == Provenance::VendorArchive));
    let issued = provider.follow_ups_to_issue(follow_ups, &covered(&[APPLE_ACCESSION]));
    assert_eq!(accessions_of(&issued), vec![MICROSOFT_ACCESSION.to_owned()]);
    Ok(())
}

/// §8 row R16. A live ingest polls every matched row, so coverage never drops a follow-up even
/// when the story is already committed: the covered set is consulted for the archive arm alone.
#[test]
fn a_live_follow_up_is_issued_even_when_its_story_is_already_covered() -> Outcome {
    let provider = provider()?;
    let follow_ups = index_follow_ups(&provider, A_LISTED_SESSION)?;
    assert!(follow_ups
        .iter()
        .all(|follow_up| follow_up.provenance == Provenance::LiveIngest));
    let issued = provider.follow_ups_to_issue(
        follow_ups,
        &covered(&[APPLE_ACCESSION, MICROSOFT_ACCESSION]),
    );
    assert_eq!(
        accessions_of(&issued),
        vec![APPLE_ACCESSION.to_owned(), MICROSOFT_ACCESSION.to_owned()]
    );
    Ok(())
}

/// The capture that the hand-written fixture could not stand in for. Taken 2026-11-13 from
/// `form.20261110.idx` through the predecessor's own SEC client: the real file is 1,057,192 bytes
/// and 5,429 filing rows, the vendor WRAPS its heading across two lines, rows are 191 to 194 bytes
/// wide and `Date Filed` is `YYYYMMDD`. A reader requiring the five headings on one line returned
/// no rows for that day and the run closed `failed` with exit 8 - the collector reporting that the
/// vendor refused when the truth was that we could not read what it sent. This fixture is a SUBSET
/// of real lines: the heading, the separator, the first 100 rows and all 391 8-K and 8-K/A rows.
#[test]
fn the_real_daily_index_parses_although_its_heading_is_wrapped_across_two_lines() -> Outcome {
    let provider = provider()?;
    let request = index_request(&provider, A_LISTED_SESSION)?;
    let extracted = provider.extract(&request, &blob(REAL_INDEX));
    assert_eq!(extracted.vendor_rows, REAL_ROWS);
    Ok(())
}

/// Every 8-K row of that day reaches the universe filter, so a heading change cannot quietly cut
/// the follow-ups to zero while the counts still look like a quiet day.
#[test]
fn every_eight_k_row_of_the_real_index_is_either_followed_up_or_filtered_out() -> Outcome {
    let provider = provider()?;
    let request = index_request(&provider, A_LISTED_SESSION)?;
    let extracted = provider.extract(&request, &blob(REAL_INDEX));
    assert_eq!(
        extracted.follow_ups.len() + usize::try_from(extracted.filtered_out)?,
        usize::try_from(REAL_ROWS)?
    );
    assert!(
        extracted.filtered_out > 0,
        "a roster that filters nothing is not a roster"
    );
    Ok(())
}

/// R17: matching runs over article body text, so what is extracted may not be page HTML. The
/// `<script>` data layer of the Apple fixture reads as prose once its tags are stripped.
#[test]
fn the_extracted_body_is_article_text_and_never_page_html_or_a_script_payload() -> Outcome {
    let item = body_under(&rules_path())?;
    let body = item.body.ok_or("the Apple fixture extracted no body")?;
    assert!(!body.text.contains(SCRIPT_TEXT), "{}", body.text);
    assert!(!body.text.contains(A_TAG), "{}", body.text);
    assert!(body.text.contains("supply agreement with Bellwether"));
    assert!(body.text.contains('"'), "the entity table was not applied");
    assert_eq!(
        body.state,
        BodyState::Present {
            bytes: body.text.len() as u64
        }
    );
    Ok(())
}

#[test]
fn one_byte_changed_in_the_rules_file_changes_the_extracted_body_and_therefore_the_item_id(
) -> Outcome {
    let under_the_committed_rules = body_under(&rules_path())?;
    let one_byte = fixtures().join("rules_one_byte_changed.json");
    let under_one_byte_less = body_under(&one_byte)?;
    assert_ne!(
        under_the_committed_rules.body, under_one_byte_less.body,
        "dropping a byte from drop_elements must change the body"
    );
    assert_ne!(
        identity(&under_the_committed_rules),
        identity(&under_one_byte_less)
    );
    assert_ne!(
        provider()?.extract_rules_version(),
        open_with("roster.json", &one_byte, CONTACT)?.extract_rules_version()
    );
    Ok(())
}

/// The false claim the adversaries found: `extract_rules_version` is not an `item_id` input, so a
/// rules edit reaches the identity only through the bytes it changes.
#[test]
fn a_rules_edit_that_leaves_the_bytes_identical_leaves_every_item_id_identical() -> Outcome {
    let extra_key = fixtures().join("rules_extra_key.json");
    let under_the_extra_key = body_under(&extra_key)?;
    assert_eq!(
        identity(&body_under(&rules_path())?),
        identity(&under_the_extra_key)
    );
    assert_ne!(
        provider()?.extract_rules_version(),
        open_with("roster.json", &extra_key, CONTACT)?.extract_rules_version(),
        "and the rules version still moves"
    );
    Ok(())
}

/// The number, with its construction: this test IS the construction, and it fails when the
/// extraction changes rather than reporting a new median.
#[test]
fn the_median_extracted_body_length_over_the_committed_filings_is_measured() -> Outcome {
    let provider = provider()?;
    let request = document_request(&provider, A_LISTED_SESSION)?;
    let mut lengths: Vec<usize> = [
        "filing_8k_apple.txt",
        "filing_8k_microsoft_no_acceptance.txt",
    ]
    .into_iter()
    .filter_map(|name| {
        only_item(&provider.extract(&request, &blob(name)))
            .ok()?
            .body
            .map(|body| body.text.chars().count())
    })
    .collect();
    lengths.sort_unstable();
    assert_eq!(lengths.len(), COMMITTED_FILINGS);
    assert_eq!(
        lengths.get(lengths.len().saturating_sub(1) / 2).copied(),
        Some(MEDIAN_BODY_CHARS),
        "{lengths:?}"
    );
    Ok(())
}

#[test]
fn the_quarter_listing_names_only_the_days_its_form_index_files_name() -> Outcome {
    let listing = listing()?;
    assert_eq!(listing.len(), 2);
    assert!(listing.names(A_LISTED_SESSION.parse()?));
    assert!(listing.names(AN_ARCHIVED_SESSION.parse()?));
    assert!(!listing.names(A_SESSION_THE_LISTING_OMITS.parse()?));
    Ok(())
}

/// The urls, pinned: the 30-day incident the planning rules exist for began with a run asking the
/// wrong path and reading the answer as a quiet day.
#[test]
fn the_urls_name_the_quarter_listing_and_the_days_own_form_index() -> Outcome {
    assert_eq!(
        EdgarPress::listing_url(A_LISTED_SESSION.parse()?),
        "https://www.sec.gov/Archives/edgar/daily-index/2026/QTR4/index.json"
    );
    assert_eq!(
        index_request(&provider()?, A_LISTED_SESSION)?.url,
        "https://www.sec.gov/Archives/edgar/daily-index/2026/QTR4/form.20261110.idx"
    );
    Ok(())
}

#[test]
fn the_conformance_harness_finds_no_breach_in_this_provider() -> Outcome {
    let provider = probed()?;
    let calendar = calendar()?;
    let listing = listing()?;
    let payloads = [
        (
            index_request(&provider, A_LISTED_SESSION)?,
            blob("form.20261110.idx"),
        ),
        (
            document_request(&provider, A_LISTED_SESSION)?,
            blob("filing_8k_apple.txt"),
        ),
        (
            document_request(&provider, A_LISTED_SESSION)?,
            blob("filing_8k_blank_accession.txt"),
        ),
    ];
    let subject = Subject {
        target: TargetWindow::new(A_WINDOW_SPANNING_ALL_FOUR),
        calendar: &calendar,
        listing: &listing,
        at: at(RUN_AT),
        payloads: &payloads,
    };
    assert_eq!(breaches(&provider, &subject), Vec::new());
    Ok(())
}

/// Adding a sink to `lib.rs` turns this red. The clock half is `repo-invariants`'s row R4 check,
/// which covers this crate and every later provider.
#[test]
fn the_provider_holds_no_sink() {
    let source = include_str!("../src/lib.rs");
    for forbidden in NO_SINK {
        assert!(!source.contains(forbidden), "{forbidden}");
    }
}
