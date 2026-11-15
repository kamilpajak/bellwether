//! A loopback server that answers one path with a 301 and every other path with 200, and a `Relay`
//! `Exchange` that sends the urls it names through a real `Wire` to that server (architecture.md
//! section 11.5 step 5). Never the network: the server binds `127.0.0.1` only.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::error::Error;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use news_provider::{Clock, Exchange, RawResponse, Request, SendFailure, SendPermit, Wire};

/// Where the 301 points: a path on the same server, so a followed hop is seen by it.
pub const REDIRECT_TARGET: &str = "/elsewhere";
const MOVED_BODY: &[u8] = b"moved";
const ACCEPT_POLL: Duration = Duration::from_millis(5);
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Bounds the accept loop of a test that returns before `Loopback::finish`.
const SERVER_DEADLINE: Duration = Duration::from_secs(60);

type Seen = Arc<Mutex<Vec<String>>>;

pub struct Loopback {
    base: String,
    seen: Seen,
    stop: Arc<AtomicBool>,
    served: JoinHandle<std::io::Result<()>>,
}

impl Loopback {
    /// Answers `moved` with `301 Location: REDIRECT_TARGET` and any other path with 200 and
    /// `arrived`, one connection at a time, each closed after its answer.
    pub fn redirecting(moved: &str, arrived: Vec<u8>) -> std::io::Result<Self> {
        Self::redirecting_with(moved, MOVED_BODY.to_vec(), arrived)
    }

    /// As `redirecting`, with `moved_body` as the 301's body.
    pub fn redirecting_with(
        moved: &str,
        moved_body: Vec<u8>,
        arrived: Vec<u8>,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let base = format!("http://{}", listener.local_addr()?);
        let seen = Seen::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (moved, recorded, stopped) = (moved.to_owned(), Arc::clone(&seen), Arc::clone(&stop));
        let answers = Answers {
            moved,
            moved_body,
            arrived,
        };
        let served = thread::spawn(move || -> std::io::Result<()> {
            let deadline = Instant::now() + SERVER_DEADLINE;
            while !stopped.load(Ordering::SeqCst) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((stream, _peer)) => answer_one(stream, &answers, &recorded)?,
                    Err(why) if why.kind() == ErrorKind::WouldBlock => thread::sleep(ACCEPT_POLL),
                    Err(why) => return Err(why),
                }
            }
            Ok(())
        });
        Ok(Self {
            base,
            seen,
            stop,
            served,
        })
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// The request targets seen since the last call, in order.
    pub fn seen(&self) -> Result<Vec<String>, Box<dyn Error>> {
        let mut seen = self
            .seen
            .lock()
            .map_err(|_| "the loopback server panicked")?;
        Ok(std::mem::take(&mut *seen))
    }

    pub fn finish(self) -> Result<(), Box<dyn Error>> {
        self.stop.store(true, Ordering::SeqCst);
        match self.served.join() {
            Ok(served) => Ok(served?),
            Err(_) => Err("the loopback server panicked".into()),
        }
    }
}

/// The path answered 301 and the two bodies.
struct Answers {
    moved: String,
    moved_body: Vec<u8>,
    arrived: Vec<u8>,
}

/// Reads one request head, records its target, and answers it. A client that stops reading a body
/// over its limit closes the socket, so the server's write of the rest may fail and is not an error.
fn answer_one(
    mut stream: TcpStream,
    answers: &Answers,
    seen: &Mutex<Vec<String>>,
) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    let mut asked = Vec::new();
    let mut byte = [0_u8; 1];
    while !asked.ends_with(b"\r\n\r\n") && stream.read(&mut byte)? == 1 {
        asked.extend_from_slice(&byte);
    }
    let target = String::from_utf8_lossy(&asked)
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let (head, body) = if target == answers.moved {
        (
            format!("301 Moved Permanently\r\nLocation: {REDIRECT_TARGET}"),
            answers.moved_body.as_slice(),
        )
    } else {
        ("200 OK".to_owned(), answers.arrived.as_slice())
    };
    if let Ok(mut recorded) = seen.lock() {
        recorded.push(target);
    }
    let len = body.len();
    stream.write_all(
        format!("HTTP/1.1 {head}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n").as_bytes(),
    )?;
    match stream.write_all(body) {
        Err(why)
            if matches!(
                why.kind(),
                ErrorKind::BrokenPipe | ErrorKind::ConnectionReset
            ) =>
        {
            Ok(())
        }
        written => written,
    }
}

/// Sends each url in `relayed` through `wire` to the url it maps to, keeping the rest of the
/// `Request`, and every other url to `stub`. Records every url it is asked for.
pub struct Relay<'stub> {
    pub stub: &'stub dyn Exchange,
    pub wire: Wire,
    pub relayed: BTreeMap<String, String>,
    pub asked: RefCell<Vec<String>>,
}

impl Relay<'_> {
    /// The urls asked for since the last call.
    pub fn sent(&self) -> Vec<String> {
        self.asked.take()
    }
}

impl Exchange for Relay<'_> {
    fn send(
        &self,
        permit: SendPermit,
        request: &Request,
        clock: &dyn Clock,
    ) -> Result<RawResponse, SendFailure> {
        self.asked.borrow_mut().push(request.url.clone());
        match self.relayed.get(&request.url) {
            Some(to) => {
                let relayed = Request {
                    url: to.clone(),
                    ..request.clone()
                };
                self.wire.send(permit, &relayed, clock)
            }
            None => self.stub.send(permit, request, clock),
        }
    }
}
