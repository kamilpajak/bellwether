//! Several OS processes sending through one pace root at the same time: `docs/design/architecture.md`
//! section 8 row R19, hole 2. One thread cannot see it, so each sender is this test binary started
//! again with `SENDER` set, running the real `Wire::new`, `Wire::reserve` and `Wire::send` against a
//! loopback listener that stamps each connection when `accept` returns.
//!
//! Deterministic by construction: the assertions are counts, orderings and lower bounds on gaps
//! between instants one clock stamped under one lock. No assertion is an upper bound on a wait.

use std::collections::BTreeMap;
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
    AttemptEvent, AttemptSlot, Clock, Credentials, HeaderValue, LedgerError, PaceRoot, Provenance,
    Request, RequestBudget, RequestPhase, RunId, SendFailure, Sink, SourceId, SystemClock,
    TargetWindow, Timeouts, TransportProfile, Wire,
};
use pit::ObservationTime;

type Checked<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// Set in a sender process only: its `Sender` fields, the listener, the pace root and its output
/// file, one per line.
const SENDER: &str = "SHARED_BUCKET_SENDER";
/// `id:interval_ms:sends:delay_ms:per_ip:host` per sender, comma-separated, for `measure`; `per_ip`
/// is `true` for the sender's interval, `false` for none, or its own milliseconds.
const MEASURE: &str = "SHARED_BUCKET_MEASURE";
const SENDER_TEST: &str = "sender";
const BUCKET: &str = "shared_bucket";
const LOOPBACK: &str = "127.0.0.1";
/// A second host on the one listener: its own per-host bucket, and the per-IP one shared.
const LOCALHOST: &str = "localhost";
const HOST_PACE: &str = "pace/127.0.0.1.per-host";
const HOST_GATE: &str = "pace/127.0.0.1.per-host.sent";
/// Far past any exchange on a loaded runner: no test here is about a timeout.
const PATIENCE: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_millis(10);
const ANSWER: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";

/// One sender process: a per-host pace at its interval, with `per_ip` a per-IP one of that many
/// milliseconds too, which is SEC's shape, and `delay_ms` between its reservation and its send,
/// which is where `collect` writes the attempt row.
#[derive(Debug, Clone, Copy)]
struct Sender {
    id: &'static str,
    interval_ms: u64,
    sends: u32,
    delay_ms: u64,
    per_ip: Option<u64>,
    host: &'static str,
}

/// One send as its process saw it: `requested` is `None` when the gate refused it, and `outcome`
/// then names the refusal; `left` is when `Wire::send` returned.
#[derive(Debug, Clone)]
struct Sent {
    id: String,
    entered: i64,
    requested: Option<i64>,
    outcome: String,
    left: i64,
}

/// One connection: when `accept` returned, how long before that the accepting thread last
/// polled, and when the request head had been read.
#[derive(Debug, Clone)]
struct Arrival {
    path: String,
    accepted: i64,
    stall: i64,
    read: i64,
}

struct Nowhere;

impl Sink for Nowhere {
    fn write(&mut self, _event: &AttemptEvent) -> Result<(), LedgerError> {
        Ok(())
    }
}

fn profile(interval: Duration, per_ip: Option<Duration>) -> TransportProfile {
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
            min_interval: per_ip,
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
    let per_ip = per_ip.parse().ok().map(Duration::from_millis);
    let wire = Wire::new(
        profile(Duration::from_millis(interval.parse()?), per_ip),
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
        // A refusal is recorded by its variant's name, the first word of its `Debug` form.
        let outcome = match wire.send(slot.take_permit()?, &asked, &clock) {
            Ok(answer) => answer.requested_at().inner().timestamp_micros().to_string(),
            Err(SendFailure::Budget(refused)) => {
                let refused = format!("{refused:?}");
                refused.split(' ').next().unwrap_or_default().to_owned()
            }
            Err(other) => return Err(other.into()),
        };
        let left = clock.now().timestamp_micros();
        writeln!(lines, "{id} {entered} {outcome} {left}")?;
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
    thread::spawn(move || {
        let mut polled = Utc::now().timestamp_micros();
        loop {
            let got = listener.accept();
            let accepted = Utc::now().timestamp_micros();
            let stall = accepted - polled;
            polled = accepted;
            let Ok((stream, _)) = got else {
                std::hint::spin_loop();
                continue;
            };
            let kept = Arc::clone(&kept);
            if stream.set_nonblocking(false).is_ok() {
                thread::spawn(move || answer(stream, (accepted, stall), &kept));
            }
        }
    });
    Ok((port, arrivals))
}

/// Recorded before the answer is written: a sender exits on reading it, and the test then reads
/// the list.
fn answer(mut stream: TcpStream, (accepted, stall): (i64, i64), kept: &Mutex<Vec<Arrival>>) {
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
    if let Ok(mut kept) = kept.lock() {
        kept.push(Arrival {
            path,
            accepted,
            stall,
            read,
        });
    }
    drop(stream.write_all(ANSWER));
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
        sender
            .per_ip
            .map_or_else(|| "-".to_owned(), |ms| ms.to_string()),
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
            let [id, entered, outcome, left] = fields.as_slice() else {
                return Err(format!("not a send: {line}").into());
            };
            Ok(Sent {
                id: (*id).to_owned(),
                entered: entered.parse()?,
                requested: outcome.parse().ok(),
                outcome: (*outcome).to_owned(),
                left: left.parse()?,
            })
        })
        .collect()
}

/// Every send of `senders` started together on one fresh pace root, and every arrival at a
/// sender's own path: another local program may connect to the port.
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
    let mut arrivals = arrivals.lock().map_err(|why| why.to_string())?.clone();
    arrivals.retain(|arrival| {
        let asked_by = |sender: &Sender| arrival.path.starts_with(&format!("/{}/", sender.id));
        senders.iter().any(asked_by)
    });
    Ok((sent, arrivals))
}

/// The gate's rule in each bucket, a host's or the per-IP one: each stamped pair whose gap is
/// below the LATER sender's interval for that bucket, as `(gap, interval)` in microseconds.
fn pairs_too_close(senders: &[Sender], sent: &[Sent]) -> Vec<(i64, i64)> {
    const PER_IP: &str = "";
    let mut buckets: BTreeMap<&str, Vec<(i64, i64)>> = BTreeMap::new();
    for send in sent {
        let sender = senders.iter().find(|sender| sender.id == send.id);
        let (Some(at), Some(sender)) = (send.requested, sender) else {
            continue;
        };
        for (bucket, ms) in [
            (sender.host, Some(sender.interval_ms)),
            (PER_IP, sender.per_ip),
        ] {
            if let Some(micros) = ms.and_then(|ms| i64::try_from(ms * 1_000).ok()) {
                buckets.entry(bucket).or_default().push((at, micros));
            }
        }
    }
    let mut close = Vec::new();
    for stamps in buckets.values_mut() {
        stamps.sort_unstable();
        close.extend(stamps.windows(2).filter_map(|pair| match pair {
            [(earlier, _), (later, interval)] if later - earlier < *interval => {
                Some((later - earlier, *interval))
            }
            _ => None,
        }));
    }
    close
}

const FAST: u64 = 50;
const SLOW: u64 = 250;
/// Past one slow interval, so the slow sender reaches the gate after the fast ones resumed.
const LATE: u64 = 325;
/// The fast senders stamp for over 3 s, past the 2 s that 8 tries of 250 ms lasted before the fix.
/// `fast-b` asks a second host, so only the per-IP gate's lock stands between it and the others;
/// `plain` owes no per-IP pace, so only the host gate's does. Each alone stamps for 3 s or more.
const SENDERS: [Sender; 4] = [
    Sender {
        id: "fast-a",
        interval_ms: FAST,
        sends: 30,
        delay_ms: 0,
        per_ip: Some(FAST),
        host: LOOPBACK,
    },
    Sender {
        id: "fast-b",
        interval_ms: FAST,
        sends: 60,
        delay_ms: 0,
        per_ip: Some(FAST),
        host: LOCALHOST,
    },
    Sender {
        id: "plain",
        interval_ms: FAST,
        sends: 60,
        delay_ms: 0,
        per_ip: None,
        host: LOOPBACK,
    },
    Sender {
        id: "slow",
        interval_ms: SLOW,
        sends: 3,
        delay_ms: LATE,
        per_ip: Some(SLOW),
        host: LOOPBACK,
    },
];
const ALL_OF_THEM: usize = 153;

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
    per_ip: Some(FAST),
    host: LOOPBACK,
};
/// Refused only after 32 of its 250 ms at the lock (`GATE_TURNS`): 8 s for the kill to come in.
const SURVIVOR: Sender = Sender {
    id: "survivor",
    interval_ms: SLOW,
    sends: 3,
    delay_ms: 0,
    per_ip: None,
    host: LOOPBACK,
};
const OTHER_HOST: Sender = Sender {
    id: "other-host",
    interval_ms: FAST,
    sends: 3,
    delay_ms: 0,
    per_ip: Some(FAST),
    host: LOCALHOST,
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

/// Polls until `seen` answers yes; an error past `PATIENCE`.
fn until(mut seen: impl FnMut() -> Checked<bool>) -> Checked {
    let mut waited = Duration::ZERO;
    while !seen()? {
        if waited > PATIENCE {
            return Err("not seen within the test's patience".into());
        }
        thread::sleep(POLL);
        waited += POLL;
    }
    Ok(())
}

/// `HOLDER` on a host gate that says a call left just now: it owes its whole hour and waits it
/// holding that gate's lock. Returned once it is seen there.
fn holder_waiting(port: u16, root: &Path) -> Checked<Running> {
    fs::create_dir(root.join("pace"))?;
    let gate = root.join(HOST_GATE);
    fs::write(
        &gate,
        Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true),
    )?;
    let holder = start(HOLDER, port, root)?;
    let mut held = 0;
    until(|| {
        held = if locked(&gate)? { held + 1 } else { 0 };
        Ok(held == HELD_FOR)
    })?;
    Ok(holder)
}

/// Killed in its wait, the holder must leave nothing the survivor waits for: the survivor's first
/// stamp is after the kill, because it queued behind the lock, and it is not an hour away.
#[test]
fn a_sender_killed_while_it_waits_at_the_gate_does_not_block_the_next() -> Checked {
    let root = tempfile::tempdir()?;
    let (port, _arrivals) = listen(false)?;
    let holder = holder_waiting(port, root.path())?;
    let pace = root.path().join(HOST_PACE);
    let reserved = fs::read_to_string(&pace)?;
    let survivor = start(SURVIVOR, port, root.path())?;
    // Its reservation on file, the gate is the survivor's next step; then time for a survivor
    // nothing blocks to stamp before the kill, so that it is red.
    until(|| Ok(fs::read_to_string(&pace)? != reserved))?;
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

/// The holder waits out its host holding that host's lock and not the per-IP gate's, so a sender
/// of the same per-IP bucket asking another host is stamped while the holder still waits. With
/// both locks held it is refused `GateHeld` after 32 of its 50 ms.
#[test]
fn a_sender_waiting_out_its_host_holds_no_lock_another_host_waits_for() -> Checked {
    let root = tempfile::tempdir()?;
    let (port, _arrivals) = listen(false)?;
    let _holder = holder_waiting(port, root.path())?;
    let other = start(OTHER_HOST, port, root.path())?;
    let sent = finished(other, OTHER_HOST, root.path())?;
    let stamped = sent.iter().filter(|send| send.requested.is_some()).count();
    assert_eq!(
        (stamped, locked(&root.path().join(HOST_GATE))?),
        (3, true),
        "{sent:?}"
    );
    Ok(())
}

fn at(micros: i64) -> String {
    DateTime::from_timestamp_micros(micros).map_or_else(String::new, |at| at.to_rfc3339())
}

const LATE_US: i64 = 3_000;
const STALL_US: i64 = 1_000;

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
/// changes: `SHARED_BUCKET_MEASURE=a:100:300:0:true:127.0.0.1,b:100:300:0:true:127.0.0.1 cargo test
/// --release -p news-provider --test shared_bucket -- --ignored --nocapture measure`.
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
        let interval_ms = interval.parse()?;
        senders.push(Sender {
            id: String::from(*id).leak(),
            interval_ms,
            sends: sends.parse()?,
            delay_ms: delay.parse()?,
            per_ip: match *per_ip {
                "true" => Some(interval_ms),
                "false" => None,
                its_own => Some(its_own.parse()?),
            },
            host: String::from(*host).leak(),
        });
    }
    let (sent, arrivals) = run(&senders, true)?;
    for sender in &senders {
        let own = sent.iter().filter(|send| send.id == sender.id);
        let mut refused = BTreeMap::new();
        for send in own.clone().filter(|send| send.requested.is_none()) {
            *refused.entry(send.outcome.as_str()).or_insert(0) += 1;
        }
        println!("{} sent {} refused {refused:?}", sender.id, own.count());
    }
    // How long a refused send was inside `Wire::send` before its named outcome.
    let mut refusals: Vec<i64> = sent
        .iter()
        .filter(|send| send.requested.is_none())
        .map(|send| send.left - send.entered)
        .collect();
    refusals.sort_unstable();
    let stamps: Vec<i64> = sent.iter().filter_map(|send| send.requested).collect();
    // The longest time from entering `Wire::send` to the stamp, and how many stamps of other
    // processes fell inside one such wait at most: a sender's place in line.
    let mut waits: Vec<(i64, &str)> = sent
        .iter()
        .filter_map(|send| Some((send.requested? - send.entered, send.id.as_str())))
        .collect();
    waits.sort_unstable();
    let places = sent.iter().filter_map(|send| {
        let own = send.requested?;
        Some(
            stamps
                .iter()
                .filter(|other| (send.entered..own).contains(*other))
                .count(),
        )
    });
    // Per send, accept minus stamp, with the accepting thread's stall: the spread is how far an
    // arrival gap can fall below a stamp gap. Paths are `/<id>/<seq>`, a sender's lines in order.
    let mut delays = Vec::new();
    for sender in &senders {
        let own = sent.iter().filter(|send| send.id == sender.id);
        for (seq, send) in own.enumerate() {
            let path = format!("/{}/{seq}", sender.id);
            let arrival = arrivals.iter().find(|arrival| arrival.path == path);
            if let (Some(requested), Some(arrival)) = (send.requested, arrival) {
                delays.push((arrival.accepted - requested, arrival.stall));
            }
        }
    }
    delays.sort_unstable();
    // Rule B5: a delay over 3 ms is the listener's when its thread had not polled for over 1 ms.
    let late = delays.iter().filter(|(delay, _)| *delay > LATE_US);
    let listeners = late.clone().filter(|(_, stall)| *stall > STALL_US).count();
    println!(
        "too close {:?}\nsmallest gap us: stamps {:?} accepted {:?} read {:?}\n\
         gate wait us: median {:?} longest {:?}\nmost stamps of others inside one wait {:?}\n\
         wait before a refusal us: shortest {:?} longest {:?}\n\
         accept minus stamp us: min {:?} median {:?} max {:?}\n\
         over 3 ms: {} of {}, the listener's {listeners}; longest stall us {:?}",
        pairs_too_close(&senders, &sent),
        smallest_gap(stamps.clone()).map(|(gap, later)| (gap, at(later))),
        smallest_gap(arrivals.iter().map(|arrival| arrival.accepted).collect()),
        smallest_gap(arrivals.iter().map(|arrival| arrival.read).collect()),
        waits.get(waits.len() / 2).map(|(wait, _)| wait),
        waits.last(),
        places.max(),
        refusals.first(),
        refusals.last(),
        delays.first().map(|(delay, _)| delay),
        delays.get(delays.len() / 2).map(|(delay, _)| delay),
        delays.last(),
        late.count(),
        delays.len(),
        delays.iter().map(|(_, stall)| stall).max(),
    );
    Ok(())
}
