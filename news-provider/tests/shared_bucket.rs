//! Several OS processes sending through one pace root at the same time: `docs/design/architecture.md`
//! section 8 row R19, hole 2. One thread cannot see it, so each sender is this test binary started
//! again with `SENDER` set, running the real `Wire::new`, `Wire::reserve` and `Wire::send` against a
//! loopback listener that stamps each connection when `accept` returns.
//!
//! Deterministic by construction: the assertions are counts, orderings and lower bounds on gaps
//! between instants one clock stamped under one lock. No assertion is an upper bound on a wait.

use std::fmt::Write as _;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use canon::FileDigest;
use chrono::{DateTime, SecondsFormat, Utc};
use news_provider::{
    AttemptEvent, AttemptSlot, BudgetError, Clock, Credentials, HeaderValue, LedgerError, PaceRoot,
    Provenance, Request, RequestBudget, RequestPhase, RunId, SendFailure, Sink, SourceId,
    SystemClock, TargetWindow, Timeouts, TransportProfile, Wire,
};
use pit::ObservationTime;

type Checked<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// Set in a sender process only: its `Sender` fields, the listener, the pace root and its output
/// file, one per line.
const SENDER: &str = "SHARED_BUCKET_SENDER";
/// `id:interval_ms:sends:delay_ms:per_ip:host` per sender, comma-separated, for `measure`.
const MEASURE: &str = "SHARED_BUCKET_MEASURE";
const SENDER_TEST: &str = "sender";
const BUCKET: &str = "shared_bucket";
const LOOPBACK: &str = "127.0.0.1";
/// A second host on the one listener: its own per-host bucket, and the per-IP one shared.
const LOCALHOST: &str = "localhost";
const HOST_GATE: &str = "pace/127.0.0.1.per-host.sent";
const REFUSED: &str = "never_cleared";
/// Far past any exchange on a loaded runner: no test here is about a timeout.
const PATIENCE: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_millis(10);
const ANSWER: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";

/// One sender process: a per-host pace at its interval, with `per_ip` a per-IP one too, which is
/// SEC's shape, and `delay_ms` between its reservation and its send, which is where `collect`
/// writes the attempt row.
#[derive(Debug, Clone, Copy)]
struct Sender {
    id: &'static str,
    interval_ms: u64,
    sends: u32,
    delay_ms: u64,
    per_ip: bool,
    host: &'static str,
}

/// One send as its process saw it: `requested` is `None` when the gate refused it.
#[derive(Debug, Clone)]
struct Sent {
    id: String,
    entered: i64,
    requested: Option<i64>,
}

/// One connection: when `accept` returned, and when its request head had been read.
#[derive(Debug, Clone)]
struct Arrival {
    path: String,
    accepted: i64,
    read: i64,
}

struct Nowhere;

impl Sink for Nowhere {
    fn write(&mut self, _event: &AttemptEvent) -> Result<(), LedgerError> {
        Ok(())
    }
}

fn profile(interval: Duration, per_ip: bool) -> TransportProfile {
    TransportProfile {
        user_agent: HeaderValue::Literal("bellwether shared-bucket test".to_owned()),
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
        body_limit: 1_024,
        budget: RequestBudget {
            min_interval: per_ip.then_some(interval),
            per_host: Some(interval),
        },
    }
}

fn request(url: String) -> Request {
    Request {
        source: SourceId::new(BUCKET),
        target_window: TargetWindow::new("2026-11-26"),
        phase: RequestPhase::Index,
        provenance: Provenance::LiveIngest,
        url,
        universe_version: FileDigest::of(b"{}"),
        listed_date: None,
        names_ticker: false,
        hosts_version: None,
        extract_version: None,
        headers: Vec::new(),
        params: Vec::new(),
    }
}

/// The sender process. Under a plain `cargo test` the variable is unset and this is empty.
#[test]
fn sender() -> Checked {
    let Ok(spec) = std::env::var(SENDER) else {
        return Ok(());
    };
    let fields: Vec<&str> = spec.lines().collect();
    let [id, interval, sends, delay, per_ip, url, root, out] = fields.as_slice() else {
        return Err(format!("not a sender: {spec}").into());
    };
    let (delay, root) = (Duration::from_millis(delay.parse()?), Path::new(root));
    let wire = Wire::new(
        profile(Duration::from_millis(interval.parse()?), per_ip.parse()?),
        Credentials::default(),
        &PaceRoot::resolve(Some(root), root)?,
        &SourceId::new(BUCKET),
    );
    let (clock, run) = (SystemClock, RunId::new(*id));
    let mut lines = String::new();
    for seq in 0..sends.parse::<u32>()? {
        let asked = request(format!("{url}/{id}/{seq}"));
        let opened = ObservationTime::new(clock.now());
        let mut slot = AttemptSlot::open(&mut Nowhere, &asked, None, &run, seq, opened)?;
        let _go = wire.reserve(&asked, &clock, None)?;
        thread::sleep(delay);
        let entered = clock.now().timestamp_micros();
        let requested = match wire.send(slot.take_permit()?, &asked, &clock) {
            Ok(answer) => answer.requested_at().inner().timestamp_micros().to_string(),
            Err(SendFailure::Budget(BudgetError::NeverCleared { .. })) => REFUSED.to_owned(),
            Err(other) => return Err(other.into()),
        };
        writeln!(lines, "{id} {entered} {requested}")?;
        fs::write(out, &lines)?;
    }
    Ok(())
}

/// Stamps on the accepting thread before anything is read, and reads on a thread of its own, so
/// one slow read cannot delay the next stamp. The threads end with the test process.
/// `spinning` polls `accept` without sleeping: a thread blocked in `accept` was woken up to 9.6 ms
/// late on the development Mac, a spinning one within 1.5 ms (300 sends each, 2026-11-26).
fn listen(spinning: bool) -> Checked<(u16, Arc<Mutex<Vec<Arrival>>>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(spinning)?;
    let port = listener.local_addr()?.port();
    let arrivals = Arc::new(Mutex::new(Vec::new()));
    let kept = Arc::clone(&arrivals);
    thread::spawn(move || loop {
        let Ok((stream, _)) = listener.accept() else {
            std::hint::spin_loop();
            continue;
        };
        let accepted = Utc::now().timestamp_micros();
        let kept = Arc::clone(&kept);
        if stream.set_nonblocking(false).is_ok() {
            thread::spawn(move || answer(stream, accepted, &kept));
        }
    });
    Ok((port, arrivals))
}

fn answer(mut stream: TcpStream, accepted: i64, kept: &Mutex<Vec<Arrival>>) {
    let (mut head, mut chunk) = (Vec::new(), [0_u8; 512]);
    while !head.windows(4).any(|end| end == b"\r\n\r\n") {
        match stream.read(&mut chunk) {
            Ok(read) if read > 0 => head.extend(chunk.get(..read).unwrap_or_default()),
            _ => return,
        }
    }
    let read = Utc::now().timestamp_micros();
    let head = String::from_utf8_lossy(&head);
    let path = head.split(' ').nth(1).unwrap_or_default().to_owned();
    if stream.write_all(ANSWER).is_ok() {
        if let Ok(mut kept) = kept.lock() {
            kept.push(Arrival {
                path,
                accepted,
                read,
            });
        }
    }
}

/// A sender process, killed when a test leaves it behind on any path.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        drop(self.0.kill().and_then(|()| self.0.wait()));
    }
}

fn start(sender: Sender, port: u16, root: &Path) -> Checked<Running> {
    let out = root.join(sender.id);
    let spec = format!(
        "{}\n{}\n{}\n{}\n{}\nhttp://{}:{port}\n{}\n{}",
        sender.id,
        sender.interval_ms,
        sender.sends,
        sender.delay_ms,
        sender.per_ip,
        sender.host,
        root.display(),
        out.display()
    );
    let child = Command::new(std::env::current_exe()?)
        .args(["--exact", SENDER_TEST, "--nocapture"])
        .env(SENDER, spec)
        .stdout(Stdio::null())
        .spawn()?;
    Ok(Running(child))
}

/// Waits for `child` and reads what it wrote; an error past `PATIENCE`.
fn finished(mut child: Running, sender: Sender, root: &Path) -> Checked<Vec<Sent>> {
    let mut waited = Duration::ZERO;
    let status = loop {
        if let Some(status) = child.0.try_wait()? {
            break status;
        }
        if waited > PATIENCE {
            return Err(format!("{} was still sending after {PATIENCE:?}", sender.id).into());
        }
        thread::sleep(POLL);
        waited += POLL;
    };
    if !status.success() {
        return Err(format!("{} exited {status}", sender.id).into());
    }
    fs::read_to_string(root.join(sender.id))?
        .lines()
        .map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            let [id, entered, requested] = fields.as_slice() else {
                return Err(format!("not a send: {line}").into());
            };
            Ok(Sent {
                id: (*id).to_owned(),
                entered: entered.parse()?,
                requested: requested.parse().ok(),
            })
        })
        .collect()
}

/// Every send of `senders` started together on one fresh pace root, and every arrival.
fn run(senders: &[Sender], spinning: bool) -> Checked<(Vec<Sent>, Vec<Arrival>)> {
    let root = tempfile::tempdir()?;
    let (port, arrivals) = listen(spinning)?;
    let mut children = Vec::new();
    for sender in senders {
        children.push((start(*sender, port, root.path())?, *sender));
    }
    let mut sent = Vec::new();
    for (child, sender) in children {
        sent.extend(finished(child, sender, root.path())?);
    }
    let arrivals = arrivals.lock().map_err(|why| why.to_string())?.clone();
    Ok((sent, arrivals))
}

fn interval_of(senders: &[Sender], id: &str) -> i64 {
    senders
        .iter()
        .find(|sender| sender.id == id)
        .map_or(0, |sender| {
            i64::try_from(sender.interval_ms * 1_000).unwrap_or(i64::MAX)
        })
}

/// The gate's rule: each stamped pair whose gap is below the LATER sender's interval, as
/// `(gap, interval)` in microseconds.
fn pairs_too_close(senders: &[Sender], sent: &[Sent]) -> Vec<(i64, i64)> {
    let mut stamped: Vec<(i64, &str)> = sent
        .iter()
        .filter_map(|send| Some((send.requested?, send.id.as_str())))
        .collect();
    stamped.sort_unstable();
    stamped
        .windows(2)
        .filter_map(|pair| match pair {
            [(earlier, _), (later, id)] => Some((later - earlier, interval_of(senders, id))),
            _ => None,
        })
        .filter(|(gap, interval)| gap < interval)
        .collect()
}

const FAST: u64 = 50;
const SLOW: u64 = 250;
/// Past one slow interval, so the slow sender reaches the gate after the fast ones resumed.
const LATE: u64 = 325;
/// The fast senders stamp for over 3 s, past the 2 s that 8 tries of 250 ms lasted before the fix.
/// `fast-b` asks a second host, so only the per-IP gate's lock stands between it and the others,
/// and alone it stamps for 3 s.
const SENDERS: [Sender; 3] = [
    Sender {
        id: "fast-a",
        interval_ms: FAST,
        sends: 30,
        delay_ms: 0,
        per_ip: true,
        host: LOOPBACK,
    },
    Sender {
        id: "fast-b",
        interval_ms: FAST,
        sends: 60,
        delay_ms: 0,
        per_ip: true,
        host: LOCALHOST,
    },
    Sender {
        id: "slow",
        interval_ms: SLOW,
        sends: 3,
        delay_ms: LATE,
        per_ip: true,
        host: LOOPBACK,
    },
];
const ALL_OF_THEM: usize = 93;

/// Before the gate held its lock through its wait, the slow sender was owed again on each of its
/// 8 tries, because a fast one had stamped inside its last 250 ms, and was refused.
#[test]
fn a_slow_sender_late_behind_two_fast_ones_is_cleared_and_every_gap_holds() -> Checked {
    let (sent, arrivals) = run(&SENDERS, false)?;
    let refused: Vec<&Sent> = sent
        .iter()
        .filter(|send| send.requested.is_none())
        .collect();
    assert_eq!(
        (
            refused.len(),
            pairs_too_close(&SENDERS, &sent),
            sent.len(),
            arrivals.len()
        ),
        (0, Vec::new(), ALL_OF_THEM, ALL_OF_THEM),
        "{refused:?}"
    );
    Ok(())
}

const AN_HOUR_MS: u64 = 3_600_000;
const HOLDER: Sender = Sender {
    id: "holder",
    interval_ms: AN_HOUR_MS,
    sends: 1,
    delay_ms: 0,
    per_ip: false,
    host: LOOPBACK,
};
const SURVIVOR: Sender = Sender {
    id: "survivor",
    interval_ms: FAST,
    sends: 3,
    delay_ms: 0,
    per_ip: false,
    host: LOOPBACK,
};
/// A stamp takes the lock for microseconds; 20 refusals 10 ms apart are one holder waiting in it.
const HELD_FOR: u32 = 20;

/// Whether another process holds the lock on the gate file right now.
fn locked(gate: &Path) -> Checked<bool> {
    let file = fs::File::open(gate)?;
    match file.try_lock() {
        Ok(()) => {
            file.unlock()?;
            Ok(false)
        }
        Err(fs::TryLockError::WouldBlock) => Ok(true),
        Err(fs::TryLockError::Error(why)) => Err(why.into()),
    }
}

/// The gate says a call left just now, so the holder owes its whole hour and waits it holding the
/// lock; hosts only, because a per-IP reservation is itself the holder's interval long. Killed there, it must leave nothing the survivor waits for: the survivor's first stamp is
/// after the kill, because it queued behind the lock, and it is not an hour away.
#[test]
fn a_sender_killed_while_it_waits_at_the_gate_does_not_block_the_next() -> Checked {
    let root = tempfile::tempdir()?;
    let (port, _arrivals) = listen(false)?;
    fs::create_dir(root.path().join("pace"))?;
    let just_now = Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true);
    let gate = root.path().join(HOST_GATE);
    fs::write(&gate, just_now)?;
    let holder = start(HOLDER, port, root.path())?;
    let (mut held, mut waited) = (0, Duration::ZERO);
    while held < HELD_FOR {
        held = if locked(&gate)? { held + 1 } else { 0 };
        thread::sleep(POLL);
        waited += POLL;
        if waited > PATIENCE {
            return Err("the holder never waited holding the gate's lock".into());
        }
    }
    let survivor = start(SURVIVOR, port, root.path())?;
    // Time for a survivor nothing blocks to stamp before the kill, so that it is red.
    thread::sleep(POLL * HELD_FOR);
    let killed_at = Utc::now().timestamp_micros();
    drop(holder);
    let sent = finished(survivor, SURVIVOR, root.path())?;
    let stamps: Vec<i64> = sent.iter().filter_map(|send| send.requested).collect();
    let first = stamps.first().copied().unwrap_or_default();
    assert_eq!(
        (stamps.len(), first >= killed_at),
        (3, true),
        "{sent:?} killed at {killed_at}"
    );
    Ok(())
}

fn at(micros: i64) -> String {
    DateTime::from_timestamp_micros(micros).map_or_else(String::new, |at| at.to_rfc3339())
}

/// The smallest gap between consecutive values, with the later one.
fn smallest_gap(mut instants: Vec<i64>) -> Option<(i64, i64)> {
    instants.sort_unstable();
    instants
        .windows(2)
        .filter_map(|pair| match pair {
            [earlier, later] => Some((later - earlier, *later)),
            _ => None,
        })
        .min()
}

/// The instrument behind section 12.6's tables. Run again when the gate, the pace or SEC's margin
/// changes: `SHARED_BUCKET_MEASURE=a:100:300:0:true:127.0.0.1,b:100:300:0:true:127.0.0.1 cargo test --release -p news-provider
/// --test shared_bucket -- --ignored --nocapture measure`.
#[test]
#[ignore = "a measurement: it prints numbers and asserts nothing"]
fn measure() -> Checked {
    let spec = std::env::var(MEASURE)?;
    let mut senders = Vec::new();
    for sender in spec.split(',') {
        let fields: Vec<&str> = sender.split(':').collect();
        let [id, interval, sends, delay, per_ip, host] = fields.as_slice() else {
            return Err(format!("not a sender: {sender}").into());
        };
        senders.push(Sender {
            id: String::from(*id).leak(),
            interval_ms: interval.parse()?,
            sends: sends.parse()?,
            delay_ms: delay.parse()?,
            per_ip: per_ip.parse()?,
            host: String::from(*host).leak(),
        });
    }
    let (sent, arrivals) = run(&senders, true)?;
    for sender in &senders {
        let own = sent.iter().filter(|send| send.id == sender.id);
        let refused = own.clone().filter(|send| send.requested.is_none()).count();
        println!("{} sent {} refused {refused}", sender.id, own.count());
    }
    let stamps: Vec<i64> = sent.iter().filter_map(|send| send.requested).collect();
    // The longest time from entering `Wire::send` to the stamp, and how many stamps of other
    // processes fell inside one such wait at most: a sender's place in line.
    let waits = sent
        .iter()
        .filter_map(|send| Some((send.requested? - send.entered, send)));
    let places = sent.iter().filter_map(|send| {
        let own = send.requested?;
        Some(
            stamps
                .iter()
                .filter(|other| (send.entered..own).contains(*other))
                .count(),
        )
    });
    // Per send, accept minus stamp: its spread is how far an arrival gap can fall below a stamp
    // gap. Paths are `/<id>/<seq>` and a sender's lines are in `seq` order.
    let mut delays = Vec::new();
    for sender in &senders {
        let own = sent.iter().filter(|send| send.id == sender.id);
        for (seq, send) in own.enumerate() {
            let path = format!("/{}/{seq}", sender.id);
            let arrival = arrivals.iter().find(|arrival| arrival.path == path);
            if let (Some(requested), Some(arrival)) = (send.requested, arrival) {
                delays.push(arrival.accepted - requested);
            }
        }
    }
    delays.sort_unstable();
    println!(
        "too close {:?}\nsmallest gap us: stamps {:?} accepted {:?} read {:?}\n\
         longest gate wait us {:?}\nmost stamps of others inside one wait {:?}\n\
         accept minus stamp us: min {:?} median {:?} max {:?}",
        pairs_too_close(&senders, &sent),
        smallest_gap(stamps.clone()).map(|(gap, later)| (gap, at(later))),
        smallest_gap(arrivals.iter().map(|arrival| arrival.accepted).collect()),
        smallest_gap(arrivals.iter().map(|arrival| arrival.read).collect()),
        waits.max_by_key(|(wait, _)| *wait),
        places.max(),
        delays.first(),
        delays.get(delays.len() / 2),
        delays.last(),
    );
    Ok(())
}
