//! The error codes this binary writes, one table read by the error object, `schema` and the exit
//! (architecture.md section 5.1), and the one error type every command returns.

use std::path::PathBuf;

use news_provider::contact::ContactError;
use news_provider::{BudgetError, LedgerError, PaceRootError};
use serde_json::{json, Value};
use vault_core::BlobError;

pub const ERROR_SCHEMA: &str = "filingvault.error/1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    InvalidArgument,
    InputAbsent,
    InputUnparseable,
    InputSchemaMismatch,
    CredentialContactMissing,
    IoFailed,
    PaceRootSplit,
    LeaseHeld,
    RobotsStopped,
    GateRefused,
    PartialCollect,
}

impl ErrorCode {
    pub const ALL: [Self; 11] = [
        Self::InvalidArgument,
        Self::InputAbsent,
        Self::InputUnparseable,
        Self::InputSchemaMismatch,
        Self::CredentialContactMissing,
        Self::IoFailed,
        Self::PaceRootSplit,
        Self::LeaseHeld,
        Self::RobotsStopped,
        Self::GateRefused,
        Self::PartialCollect,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::InvalidArgument => "invalid_argument",
            Self::InputAbsent => "input_absent",
            Self::InputUnparseable => "input_unparseable",
            Self::InputSchemaMismatch => "input_schema_mismatch",
            Self::CredentialContactMissing => "credential_contact_missing",
            Self::IoFailed => "io_failed",
            Self::PaceRootSplit => "pace_root_split",
            Self::LeaseHeld => "lease_held",
            Self::RobotsStopped => "robots_stopped",
            Self::GateRefused => "gate_refused",
            Self::PartialCollect => "partial_collect",
        }
    }

    #[must_use]
    pub const fn exit(self) -> u8 {
        match self {
            Self::InputAbsent => 4,
            Self::LeaseHeld | Self::RobotsStopped | Self::GateRefused => 7,
            Self::PartialCollect => 8,
            _ => 2,
        }
    }

    /// 7 is the retryable class, and only it.
    #[must_use]
    pub const fn retryable(self) -> bool {
        self.exit() == 7
    }

    /// The code a `news-provider` error names by its section 5.1 text.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|code| code.name() == name)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FilingsError {
    #[error("{0}")]
    Usage(String),
    #[error("{} does not exist", .0.display())]
    Absent(PathBuf),
    #[error("{what} could not be read: {why}")]
    Unparseable { what: String, why: String },
    #[error("{} is at user_version {found}; this build reads {expected}", .path.display())]
    SchemaMismatch {
        path: PathBuf,
        found: i64,
        expected: i64,
    },
    #[error(transparent)]
    Contact(#[from] ContactError),
    /// SEC answers a User-Agent with no contact address with 403 (`sec_edgar::CONTACT_CREDENTIAL`).
    #[error("the contact holds no address: set BELLWETHER_FEED_CONTACT_FILE")]
    ContactUnaddressed,
    #[error("no credential named {0} is held")]
    Credential(String),
    #[error(transparent)]
    PaceRoot(#[from] PaceRootError),
    #[error("{} is held by another writer", .0.display())]
    LeaseHeld(PathBuf),
    /// The report of the run so far rides in `details`.
    #[error("robots.txt of www.sec.gov stops the run: {reason}")]
    RobotsStopped { reason: &'static str, report: Value },
    #[error("the send gate refused attempt {attempt_id} before the socket: {why}")]
    GateRefused {
        attempt_id: String,
        why: BudgetError,
        report: Value,
    },
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Blob(#[from] BlobError),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl FilingsError {
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Usage(_) => ErrorCode::InvalidArgument,
            Self::Absent(_) => ErrorCode::InputAbsent,
            Self::Unparseable { .. } => ErrorCode::InputUnparseable,
            Self::SchemaMismatch { .. } => ErrorCode::InputSchemaMismatch,
            Self::Contact(why) => ErrorCode::named(why.code()).unwrap_or(ErrorCode::IoFailed),
            Self::ContactUnaddressed | Self::Credential(_) => ErrorCode::CredentialContactMissing,
            Self::PaceRoot(why) => ErrorCode::named(why.code()).unwrap_or(ErrorCode::IoFailed),
            Self::LeaseHeld(_) => ErrorCode::LeaseHeld,
            Self::RobotsStopped { .. } => ErrorCode::RobotsStopped,
            Self::GateRefused { .. } => ErrorCode::GateRefused,
            Self::Budget(_) | Self::Sqlite(_) | Self::Blob(_) | Self::Ledger(_) | Self::Io(_) => {
                ErrorCode::IoFailed
            }
        }
    }

    /// The `filingvault.error/1` object, written to stderr with stdout left empty.
    #[must_use]
    pub fn object(&self, argv: &[String]) -> Value {
        let code = self.code();
        let details = match self {
            Self::RobotsStopped { reason, report } => json!({"reason": reason, "report": report}),
            Self::GateRefused {
                attempt_id,
                why,
                report,
            } => json!({"attempt_id": attempt_id, "refusal": why.to_string(), "report": report}),
            Self::PaceRoot(
                PaceRootError::Split { files, .. } | PaceRootError::Nested { files, .. },
            ) => {
                json!({"files": files})
            }
            _ => json!({}),
        };
        let suggestions: Vec<&[String]> = if code.retryable() {
            vec![argv]
        } else {
            Vec::new()
        };
        json!({
            "schema": ERROR_SCHEMA,
            "code": code.name(),
            "message": self.to_string(),
            "retryable": code.retryable(),
            "details": details,
            "suggestions": suggestions,
        })
    }
}
