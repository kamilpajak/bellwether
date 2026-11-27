//! Section 8 row R19, hole 2, for the filings caller: its `Wire` reserves in the deployable's one
//! pace root under SEC's one per-IP bucket, beside the roster's. `Wire::reserve` writes the pace
//! files and opens no socket.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use clap::Parser;
use filingvault::cli::{Cli, Command, FilingsAction};
use filingvault::collect::{transport, wire_for};
use filingvault::discover::{index_ask, request};
use filingvault::store::{Ask, Kind};
use news_provider::{Clock, Credentials, PaceRoot, Provenance, Reservation, SourceId, Wire};
use sec_edgar::Caller;
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const CONTACT: &str = "bellwether collector contact@example.test";
const AT: &str = "2026-11-12T06:10:00Z";
const ROSTER_URL: &str = "https://www.sec.gov/files/company_tickers_exchange.json";

struct Fixed(DateTime<Utc>);

impl Clock for Fixed {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}

/// The pace root the binary resolves from its own option.
fn resolved(store: &Path, root: &Path) -> Result<PaceRoot, Box<dyn std::error::Error>> {
    let cli = Cli::try_parse_from([
        "filingvault",
        "filings",
        "collect",
        "--store",
        &store.display().to_string(),
        "--pace-root",
        &root.display().to_string(),
        "--rules",
        "rules.json",
    ])?;
    let Command::Filings {
        action: FilingsAction::Collect(args),
    } = cli.command
    else {
        return Err("not filings collect".into());
    };
    Ok(args.pace.resolve(&args.store)?)
}

fn filings_request() -> news_provider::Request {
    let forms = ["10-K".to_owned()];
    request(
        &index_ask(
            chrono::NaiveDate::from_ymd_opt(2026, 11, 10).unwrap_or_default(),
            &forms,
        ),
        Provenance::VendorArchive,
    )
}

/// The filings reservation at T leaves a roster one, at T with a 50 ms deadline, past it: one
/// per-host file for both. Mutation 2026-11-27, the option module resolving
/// `PaceRoot::resolve(None, store)`: red here.
#[test]
fn a_filings_request_and_a_roster_request_reserve_in_one_per_host_file() -> TestResult {
    let store = TempDir::new()?;
    let root = TempDir::new()?;
    let pace_root = resolved(store.path(), root.path())?;
    let filings = wire_for(CONTACT, &pace_root);
    let roster = Wire::new(
        sec_edgar::transport(Caller::Roster, 1),
        Credentials::default().with(sec_edgar::CONTACT_CREDENTIAL, CONTACT),
        &PaceRoot::resolve(Some(root.path()), store.path())?,
        &SourceId::new(sec_edgar::PER_IP_BUCKET),
    );
    let at: DateTime<Utc> = AT.parse()?;
    let first = filings.reserve(&filings_request(), &Fixed(at), None)?;
    let roster_ask = Ask {
        kind: Kind::Robots,
        target: sec_edgar::HOST.to_owned(),
        url: ROSTER_URL.to_owned(),
        forms: None,
        body_limit: 1,
    };
    let second = roster.reserve(
        &request(&roster_ask, Provenance::LiveIngest),
        &Fixed(at),
        Some(at + TimeDelta::milliseconds(50)),
    )?;
    assert_eq!(
        (
            first,
            matches!(second, Reservation::PastDeadline { .. }),
            root.path().join("pace/www.sec.gov.per-host").exists(),
        ),
        (Reservation::Go, true, true)
    );
    Ok(())
}

/// SEC counts requests per address: the filings caller pays into `edgar_press.per-ip` under the
/// shared root and keeps nothing under the store. Mutation 2026-11-27, the `Wire` built under
/// `edgar_filings`: red here.
#[test]
fn a_filings_request_pays_into_the_shared_per_ip_bucket() -> TestResult {
    let store = TempDir::new()?;
    let root = TempDir::new()?;
    let wire = wire_for(CONTACT, &resolved(store.path(), root.path())?);
    wire.reserve(&filings_request(), &Fixed(AT.parse()?), None)?;
    assert_eq!(
        (
            root.path().join("pace/edgar_press.per-ip").exists(),
            root.path().join("pace/edgar_filings.per-ip").exists(),
            store.path().join("pace").exists(),
        ),
        (true, false, false)
    );
    Ok(())
}

/// Mutation 2026-11-27, `filingvault` handed `Caller::Roster`: red here.
#[test]
fn the_filings_wire_takes_secs_filings_row() {
    let budget = transport().budget;
    let filings = Some(Duration::from_millis(500));
    assert_eq!((budget.min_interval, budget.per_host), (filings, filings));
}
