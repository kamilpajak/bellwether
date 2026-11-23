//! The attempt ledger's write half and the capability for the socket. An [`AttemptSlot`] exists
//! only once its `opened` row is in the store, and a [`SendPermit`] comes out of nothing but that
//! slot, so one model call needs one written row (`docs/design/architecture.md` section 8 row
//! MG1). The pattern is `news_provider::ledger`'s.

use chrono::{DateTime, Utc};

use crate::response::Reading;
use crate::store::{Store, StoreError};
use crate::wire::Exchanged;

/// What an `opened` row states.
#[derive(Debug, Clone, Copy)]
pub struct Opening<'a> {
    pub run_id: &'a str,
    /// The attempt's position within its run: a retry is a second attempt.
    pub ordinal: u32,
    pub idempotency_key: &'a str,
    /// Counted as spent until the attempt closes with the vendor's own cost.
    pub worst_case_micro_usd: i64,
    pub at: DateTime<Utc>,
}

/// An attempt whose `opened` row is written.
#[derive(Debug)]
#[must_use = "a dropped slot leaves an opened row for the next run's sweep"]
pub struct AttemptSlot {
    attempt_id: String,
    permit: Option<SendPermit>,
}

/// The capability for one send: its field is private and no constructor is public, so the only
/// route to one from outside this module is [`AttemptSlot::take_permit`].
#[derive(Debug)]
#[must_use]
pub struct SendPermit {
    attempt_id: String,
}

impl SendPermit {
    pub(crate) fn into_attempt_id(self) -> String {
        self.attempt_id
    }
}

impl AttemptSlot {
    /// The only constructor, and it writes before it returns.
    pub fn open(store: &Store, opening: &Opening) -> Result<Self, StoreError> {
        let attempt_id = format!("{}#{}", opening.run_id, opening.ordinal);
        store.write_opened(
            &attempt_id,
            opening.idempotency_key,
            opening.run_id,
            opening.worst_case_micro_usd,
            opening.at,
        )?;
        Ok(Self {
            permit: Some(SendPermit {
                attempt_id: attempt_id.clone(),
            }),
            attempt_id,
        })
    }

    pub fn take_permit(&mut self) -> Result<SendPermit, StoreError> {
        self.permit.take().ok_or(StoreError::PermitAlreadyTaken)
    }

    /// Stores the response bytes, then writes `answered`, then `finalised`. The blob is durable
    /// before the row that names it, and it is kept whatever `reading` made of it.
    pub fn close(
        self,
        store: &Store,
        exchanged: &Exchanged,
        reading: &Reading,
        at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        if exchanged.attempt_id() != self.attempt_id {
            return Err(StoreError::AnswerFromAnotherAttempt {
                slot: self.attempt_id,
                answered: exchanged.attempt_id().to_owned(),
            });
        }
        let (status, failure, response_sha256) = match &exchanged.answer {
            Ok(answer) => (
                Some(answer.status),
                None,
                Some(store.put_blob(&answer.body)?),
            ),
            Err(kind) => (None, Some(*kind), None),
        };
        store.write_answered(
            &self.attempt_id,
            exchanged.requested_at,
            exchanged.received_at,
            status,
            failure,
            response_sha256.as_deref(),
        )?;
        store.write_finalised(&self.attempt_id, reading, at)
    }
}
