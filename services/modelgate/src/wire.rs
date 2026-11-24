//! The only socket in this crate, and the only place the API key's value is read. A send consumes
//! a [`SendPermit`], each request kind's endpoint is a constant, redirects are off, and every
//! timeout is stated.
//! `docs/design/architecture.md` section 8 rows MG1 and MG5.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::ledger::SendPermit;

/// A public vendor address, not our infrastructure. No constructor outside this file's test
/// module takes another: a configurable endpoint is a switch that sends the key elsewhere.
pub const ENDPOINT: &str = "https://openrouter.ai/api/v1/systemone";
/// The `embed` kind's address, the second and last one a wire can be built for.
pub const EMBED_ENDPOINT: &str = "https://openrouter.ai/api/v1/embeddings";
/// Names the file holding the key. There is no variable for the value itself: a process
/// environment is readable through `/proc` and `docker inspect`.
pub const KEY_FILE_VARIABLE: &str = "BELLWETHER_MODEL_KEY_FILE";
/// The credential's name, which is what rows and envelopes may carry.
pub const KEY_NAME: &str = "model_key";
const GROUP_AND_WORLD: u32 = 0o077;
/// A decide answer is a few kilobytes of JSON; more than this is not one.
const RESPONSE_LIMIT_BYTES: u64 = 1 << 20;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// No embed call has been timed (2026-11-23); `embed run` starts none later than this before
/// its deadline.
pub const EMBED_CALL_TIMEOUT: Duration = Duration::from_secs(20);
const RETRY_AFTER: &str = "retry-after";

/// Why no key was read. No variant carries the file's bytes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error("{KEY_FILE_VARIABLE} is not set")]
    Unset,
    #[error("{KEY_FILE_VARIABLE} names {}, and no file is there", .0.display())]
    Absent(PathBuf),
    #[error("{KEY_FILE_VARIABLE} names {}, which could not be read: {why}", path.display())]
    Unreadable { path: PathBuf, why: String },
    #[error("{KEY_FILE_VARIABLE} names {}, which holds no key", .0.display())]
    Empty(PathBuf),
    #[error("{KEY_FILE_VARIABLE} names {}, whose key holds a control character or a space", .0.display())]
    ControlCharacter(PathBuf),
    #[error("{KEY_FILE_VARIABLE} names {}, readable by group or others (mode {mode:o}); chmod 600 it", path.display())]
    Exposed { path: PathBuf, mode: u32 },
}

impl KeyError {
    /// The section 5.1 code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Unset | Self::Empty(_) => "credential_key_missing",
            Self::Absent(_) => "input_absent",
            Self::Unreadable { .. } => "io_failed",
            Self::ControlCharacter(_) => "input_unparseable",
            Self::Exposed { .. } => "credential_key_exposed",
        }
    }
}

/// The key. No `Display`, no `Serialize`, no `Clone`, and a `Debug` that prints no byte of it.
pub struct ApiKey(String);

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

impl ApiKey {
    /// The key from the file [`KEY_FILE_VARIABLE`] names, one trailing `\n` removed.
    pub fn read(env: impl Fn(&str) -> Option<OsString>) -> Result<Self, KeyError> {
        let path = PathBuf::from(env(KEY_FILE_VARIABLE).ok_or(KeyError::Unset)?);
        let meta = match std::fs::metadata(&path) {
            Ok(meta) => meta,
            Err(why) if why.kind() == std::io::ErrorKind::NotFound => {
                return Err(KeyError::Absent(path))
            }
            Err(why) => {
                return Err(KeyError::Unreadable {
                    path,
                    why: why.to_string(),
                })
            }
        };
        let mode = meta.permissions().mode() & 0o777;
        if mode & GROUP_AND_WORLD != 0 {
            return Err(KeyError::Exposed { path, mode });
        }
        let text = match std::fs::read(&path).map(String::from_utf8) {
            Ok(Ok(text)) => text,
            Ok(Err(_)) => {
                return Err(KeyError::Unreadable {
                    path,
                    why: "not UTF-8".to_owned(),
                })
            }
            Err(why) => {
                return Err(KeyError::Unreadable {
                    path,
                    why: why.to_string(),
                })
            }
        };
        let value = text.strip_suffix('\n').unwrap_or(&text);
        if value.is_empty() {
            return Err(KeyError::Empty(path));
        }
        if value.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(KeyError::ControlCharacter(path));
        }
        Ok(Self(value.to_owned()))
    }
}

/// A status and the bytes that came with it, whatever the status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpAnswer {
    pub status: u16,
    pub body: Vec<u8>,
    /// The `Retry-After` header when it is a whole number of seconds.
    pub retry_after_seconds: Option<u64>,
}

/// One finished conversation. Built only from a consumed permit, so it names its attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exchanged {
    attempt_id: String,
    pub requested_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    /// `Err` names the kind of transport failure and never carries the client's own text.
    pub answer: Result<HttpAnswer, &'static str>,
}

impl Exchanged {
    pub fn new(
        permit: SendPermit,
        requested_at: DateTime<Utc>,
        received_at: DateTime<Utc>,
        answer: Result<HttpAnswer, &'static str>,
    ) -> Self {
        Self {
            attempt_id: permit.into_attempt_id(),
            requested_at,
            received_at,
            answer,
        }
    }

    #[must_use]
    pub fn attempt_id(&self) -> &str {
        &self.attempt_id
    }
}

/// The seam `shadow run` sends through. [`ModelWire`] is the only implementation with a socket.
pub trait ModelExchange {
    fn send(&self, permit: SendPermit, body: &[u8]) -> Exchanged;
}

pub struct ModelWire {
    agent: ureq::Agent,
    key: ApiKey,
    endpoint: String,
}

fn agent(call_timeout: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(call_timeout))
        .timeout_connect(Some(CONNECT_TIMEOUT))
        // A non-2xx carries the vendor's reason in its body, which is stored like any answer.
        .http_status_as_error(false)
        // A redirect would carry the Authorization header to an address this file does not name.
        .max_redirects(0)
        .max_redirects_will_error(false)
        .build();
    ureq::Agent::new_with_config(config)
}

fn kind_of(failure: &ureq::Error) -> &'static str {
    match failure {
        ureq::Error::Timeout(_) => "timeout",
        ureq::Error::Io(_) => "io",
        ureq::Error::HostNotFound => "host_not_found",
        ureq::Error::ConnectionFailed => "connection_failed",
        ureq::Error::Tls(_) => "tls",
        ureq::Error::BodyExceedsLimit(_) => "body_over_limit",
        _ => "other",
    }
}

impl ModelWire {
    #[must_use]
    pub fn new(key: ApiKey) -> Self {
        Self {
            agent: agent(CALL_TIMEOUT),
            key,
            endpoint: ENDPOINT.to_owned(),
        }
    }

    /// The wire of the `embed` kind: its own constant address and its own call timeout.
    #[must_use]
    pub fn embed(key: ApiKey) -> Self {
        Self {
            agent: agent(EMBED_CALL_TIMEOUT),
            key,
            endpoint: EMBED_ENDPOINT.to_owned(),
        }
    }

    /// The one read of the key's value.
    fn converse(&self, body: &[u8]) -> Result<HttpAnswer, &'static str> {
        let mut response = self
            .agent
            .post(&self.endpoint)
            .header("Authorization", format!("Bearer {}", self.key.0))
            .header("Content-Type", "application/json")
            .send(body)
            .map_err(|failure| kind_of(&failure))?;
        let retry_after_seconds = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|text| text.trim().parse().ok());
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(RESPONSE_LIMIT_BYTES)
            .read_to_vec()
            .map_err(|failure| kind_of(&failure))?;
        Ok(HttpAnswer {
            status,
            body,
            retry_after_seconds,
        })
    }
}

impl ModelExchange for ModelWire {
    fn send(&self, permit: SendPermit, body: &[u8]) -> Exchanged {
        let requested_at = Utc::now();
        let answer = self.converse(body);
        Exchanged::new(permit, requested_at, Utc::now(), answer)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::ffi::OsString;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use tempfile::TempDir;

    use super::{agent, ApiKey, KeyError, ModelWire, CALL_TIMEOUT, KEY_FILE_VARIABLE};

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    /// A value no other text in this crate holds, so finding it anywhere is a leak.
    pub(crate) const SENTINEL_KEY: &str = "sk-sentinel-7f3a9c1e5b2d";

    impl ModelWire {
        /// A wire to a loopback listener. Here and not above: this module is the file's one
        /// `#[cfg(test)]` item, so no build of the binary holds a second endpoint.
        pub(crate) fn loopback(key: ApiKey, endpoint: String) -> Self {
            Self {
                agent: agent(CALL_TIMEOUT),
                key,
                endpoint,
            }
        }
    }

    pub(crate) fn sentinel() -> ApiKey {
        ApiKey(SENTINEL_KEY.to_owned())
    }

    /// One scripted answer per connection, in order; the heads received come back on join.
    pub(crate) struct Loopback {
        pub(crate) url: String,
        served: std::thread::JoinHandle<Vec<String>>,
    }

    impl Loopback {
        /// Each answer is a full HTTP/1.1 response text. An empty one closes the connection
        /// without a byte, which the client reads as a transport failure.
        pub(crate) fn serving(answers: Vec<String>) -> TestResult<Self> {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let url = format!("http://{}/api/v1/systemone", listener.local_addr()?);
            let served = std::thread::spawn(move || {
                let mut heads = Vec::new();
                for answer in answers {
                    let Ok((mut stream, _)) = listener.accept() else {
                        break;
                    };
                    heads.push(read_request(&mut stream));
                    if !answer.is_empty() && stream.write_all(answer.as_bytes()).is_err() {
                        break;
                    }
                }
                heads
            });
            Ok(Self { url, served })
        }

        pub(crate) fn requests(self) -> Vec<String> {
            self.served.join().unwrap_or_default()
        }
    }

    /// The request head and body as text: read until the declared `content-length` is in.
    fn read_request(stream: &mut std::net::TcpStream) -> String {
        let mut seen = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let text = String::from_utf8_lossy(&seen).to_lowercase();
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let declared = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if body.len() >= declared {
                    break;
                }
            }
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => seen.extend_from_slice(&chunk[..read]),
            }
        }
        String::from_utf8_lossy(&seen).into_owned()
    }

    pub(crate) fn http(status: &str, extra_headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
             connection: close\r\n{extra_headers}\r\n{body}",
            body.len()
        )
    }

    fn key_file(dir: &Path, bytes: &[u8], mode: u32) -> TestResult<std::path::PathBuf> {
        let path = dir.join("model-key");
        std::fs::write(&path, bytes)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
        Ok(path)
    }

    fn read_at(path: &Path) -> Result<ApiKey, KeyError> {
        let value = OsString::from(path);
        ApiKey::read(|name| (name == KEY_FILE_VARIABLE).then(|| value.clone()))
    }

    #[test]
    fn a_key_file_is_read_with_one_trailing_newline_removed() -> TestResult {
        let dir = TempDir::new()?;
        let path = key_file(dir.path(), format!("{SENTINEL_KEY}\n").as_bytes(), 0o600)?;
        assert_eq!(read_at(&path)?.0, SENTINEL_KEY);
        Ok(())
    }

    /// Section 8 row MG5. Mutation 2026-11-23, the mode check removed: red on the 0o640 case.
    #[test]
    fn each_unusable_key_file_is_refused_with_its_own_code_and_no_byte_of_the_key() -> TestResult {
        let dir = TempDir::new()?;
        let unset = ApiKey::read(|_| None).err();
        let absent = read_at(&dir.path().join("nothing")).err();
        let cases: [(&[u8], u32); 5] = [
            (b"", 0o600),
            (b"\n", 0o600),
            (b"sk-sentinel-7f3a9c1e5b2d\nsecond", 0o600),
            (b"sk-sentinel-7f3a9c1e5b2d", 0o640),
            (b"\xff\xfe", 0o600),
        ];
        let mut refusals = vec![unset, absent];
        for (bytes, mode) in cases {
            refusals.push(read_at(&key_file(dir.path(), bytes, mode)?).err());
        }
        let codes: Vec<&str> = refusals.iter().flatten().map(KeyError::code).collect();
        assert_eq!(
            codes,
            [
                "credential_key_missing",
                "input_absent",
                "credential_key_missing",
                "credential_key_missing",
                "input_unparseable",
                "credential_key_exposed",
                "io_failed",
            ]
        );
        let shown = format!(
            "{refusals:?} {:?}",
            refusals
                .iter()
                .flatten()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        );
        assert!(!shown.contains("sentinel"), "{shown}");
        Ok(())
    }

    /// Section 8 row MG5. Mutation 2026-11-23, `#[derive(Debug)]` on `ApiKey`: red.
    /// Mutation 2026-11-24, `EMBED_ENDPOINT` given the `decide` path: red.
    #[test]
    fn each_wire_holds_its_own_kinds_address() {
        let (decide, embed) = (ModelWire::new(sentinel()), ModelWire::embed(sentinel()));
        assert_eq!(decide.endpoint, "https://openrouter.ai/api/v1/systemone");
        assert_eq!(embed.endpoint, "https://openrouter.ai/api/v1/embeddings");
        let design = include_str!("../../../docs/design/architecture.md");
        assert!(design.contains("| endpoint | `https://openrouter.ai/api/v1/embeddings`"));
    }

    #[test]
    fn the_keys_debug_form_prints_no_byte_of_it() {
        assert_eq!(format!("{:?}", sentinel()), "ApiKey(<redacted>)");
    }
}
