//! A ledgered robots fetch, through `cmd_robots::run` over a stub, so an open-web collect passes
//! §8 row R4(b). One url is answered; every other url fails in transport.

use std::error::Error;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use news_provider::{
    Clock, Exchange, RawResponse, Request, SendFailure, SendPermit, TransportError,
};
use newsvault::cmd_robots::{self, Ask};
use pit::ObservationTime;

const GNW_ROBOTS: &str = "https://rss.globenewswire.com/robots.txt";
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

struct Gnw {
    status: u16,
    body: Vec<u8>,
}

impl Exchange for Gnw {
    fn send(
        &self,
        permit: SendPermit,
        request: &Request,
        clock: &dyn Clock,
    ) -> Result<RawResponse, SendFailure> {
        let attempt = permit.attempt_id().clone();
        let at = ObservationTime::new(clock.now());
        if request.url == GNW_ROBOTS {
            Ok(RawResponse::new(
                attempt,
                self.status,
                self.body.clone(),
                at,
                at,
            ))
        } else {
            Err(SendFailure::Transport(TransportError::new(
                attempt,
                at,
                at,
                format!("no stub answers {}", request.url),
            )))
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
    let ask = Ask {
        source: open_web_feed::SOURCE.to_owned(),
        hosts: hosts.to_path_buf(),
        rules: provider_dir().join("rules/open-web-feed.rules.json"),
        store: store.to_path_buf(),
        contact: CONTACT.to_owned(),
        dry_run: false,
    };
    let feed = cmd_robots::open(&ask)?;
    let exchange = Gnw {
        status: answer.0,
        body: answer.1.to_vec(),
    };
    Ok(cmd_robots::run(&ask, &feed, &exchange, &At(at.parse()?))?)
}
