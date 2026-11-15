//! A ledgered robots fetch, through `cmd_robots::run` over a stub, so an open-web collect passes
//! §8 row R4(b). The urls named are answered; every other url fails in transport.

use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use news_provider::{
    Clock, Exchange, RawResponse, Request, SendFailure, SendPermit, TransportError,
};
use newsvault::cmd_robots::{self, Ask};
use pit::ObservationTime;

pub const GNW_ROBOTS: &str = "https://rss.globenewswire.com/robots.txt";
const CONTACT: &str = "bellwether collector contact@example.test";

fn provider_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../providers/open-web-feed")
}

/// The committed 2026-11-14 body the committed hosts file pins.
pub fn gnw_robots() -> Result<Vec<u8>, std::io::Error> {
    std::fs::read(provider_dir().join("tests/fixtures/gnw/robots_rss_globenewswire_com.txt"))
}

struct At(DateTime<Utc>);

impl Clock for At {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}

struct Answers(BTreeMap<String, (u16, Vec<u8>)>);

impl Exchange for Answers {
    fn send(
        &self,
        permit: SendPermit,
        request: &Request,
        clock: &dyn Clock,
    ) -> Result<RawResponse, SendFailure> {
        let attempt = permit.attempt_id().clone();
        let at = ObservationTime::new(clock.now());
        match self.0.get(&request.url) {
            Some((status, body)) => Ok(RawResponse::new(attempt, *status, body.clone(), at, at)),
            None => Err(SendFailure::Transport(TransportError::new(
                attempt,
                at,
                at,
                format!("no stub answers {}", request.url),
            ))),
        }
    }
}

/// One `robots refresh` of `hosts` into `store` at `at`, GNW's robots.txt answered `answer`.
pub fn refresh(
    store: &Path,
    hosts: &Path,
    at: &str,
    answer: (u16, &[u8]),
) -> Result<cmd_robots::Answer, Box<dyn Error>> {
    refresh_answering(store, hosts, at, &[(GNW_ROBOTS, answer.0, answer.1)])
}

/// One `robots refresh` of `hosts` into `store` at `at`, each `(url, status, body)` answered.
pub fn refresh_answering(
    store: &Path,
    hosts: &Path,
    at: &str,
    answers: &[(&str, u16, &[u8])],
) -> Result<cmd_robots::Answer, Box<dyn Error>> {
    let ask = Ask {
        source: open_web_feed::SOURCE.to_owned(),
        hosts: hosts.to_path_buf(),
        rules: provider_dir().join("rules/open-web-feed.rules.json"),
        store: store.to_path_buf(),
        contact: CONTACT.to_owned(),
        dry_run: false,
    };
    let feed = cmd_robots::open(&ask)?;
    let exchange = Answers(
        answers
            .iter()
            .map(|(url, status, body)| ((*url).to_owned(), (*status, body.to_vec())))
            .collect(),
    );
    Ok(cmd_robots::run(&ask, &feed, &exchange, &At(at.parse()?))?)
}
