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

    /// Stores the response bytes whatever `reading` made of them, then writes `answered` and
    /// `finalised` in one transaction: an `answered` row alone would be swept as `abandoned` and
    /// its request paid for again with the answer on disk.
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
        let (status, failure, response) = match &exchanged.answer {
            Ok(answer) => (
                Some(answer.status),
                None,
                Some(store.put_blob(&answer.body)?),
            ),
            Err(kind) => (None, Some(*kind), None),
        };
        store.atomically(|| {
            store.write_answered(
                &self.attempt_id,
                exchanged.requested_at,
                exchanged.received_at,
                status,
                failure,
                response.as_ref(),
            )?;
            store.write_finalised(&self.attempt_id, reading, at)
        })
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use rusqlite::Connection;
    use tempfile::TempDir;

    use super::{AttemptSlot, Opening};
    use crate::response::{Outcome, Reading};
    use crate::store::{Store, StoreError, DB_FILE};
    use crate::wire::{Exchanged, HttpAnswer};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const READING: Reading = Reading {
        outcome: Outcome::Ok,
        cost_micro_usd: None,
        answering_model: None,
    };

    fn open(store: &Store, ordinal: u32) -> Result<AttemptSlot, StoreError> {
        let opening = Opening {
            run_id: "run",
            ordinal,
            idempotency_key: "k",
            worst_case_micro_usd: 7,
            at: DateTime::<Utc>::UNIX_EPOCH,
        };
        AttemptSlot::open(store, &opening)
    }

    fn rows(root: &TempDir, event: &str) -> Result<i64, rusqlite::Error> {
        Connection::open(root.path().join(DB_FILE))?.query_row(
            "SELECT COUNT(*) FROM attempt_events WHERE event = ?1",
            [event],
            |row| row.get(0),
        )
    }

    /// Section 8 row MG1. Mutations 2026-11-23, each red: `take_permit` handing out a permit
    /// every time; `close` accepting an answer of another attempt.
    #[test]
    fn a_slot_gives_one_permit_and_closes_only_on_its_own_answer() -> TestResult {
        let root = TempDir::new()?;
        let store = Store::open(root.path())?;
        let (mut first, mut second) = (open(&store, 0)?, open(&store, 1)?);
        let permit = first.take_permit()?;
        assert!(matches!(
            first.take_permit(),
            Err(StoreError::PermitAlreadyTaken)
        ));
        let at = DateTime::<Utc>::UNIX_EPOCH;
        let answer = HttpAnswer {
            status: 200,
            body: b"{}".to_vec(),
            retry_after_seconds: None,
        };
        let of_first = Exchanged::new(permit, at, at, Ok(answer));
        assert!(matches!(second.take_permit().map(|_unused| ()), Ok(())));
        assert!(matches!(
            second.close(&store, &of_first, &READING, at),
            Err(StoreError::AnswerFromAnotherAttempt { .. })
        ));
        assert_eq!(
            (rows(&root, "answered")?, rows(&root, "finalised")?),
            (0, 0)
        );
        first.close(&store, &of_first, &READING, at)?;
        assert_eq!(
            (rows(&root, "answered")?, rows(&root, "finalised")?),
            (1, 1)
        );
        Ok(())
    }

    /// Mutation 2026-11-23, `close` writing its two rows outside `Store::atomically`: red (the
    /// `answered` row stays when `finalised` is refused).
    #[test]
    fn an_answered_row_is_never_left_without_its_finalised_row() -> TestResult {
        let root = TempDir::new()?;
        let store = Store::open(root.path())?;
        let mut slot = open(&store, 0)?;
        let at = DateTime::<Utc>::UNIX_EPOCH;
        let exchanged = Exchanged::new(slot.take_permit()?, at, at, Err("timeout"));
        // A standing `finalised` row makes the second insert fail on its primary key.
        store.write_finalised("run#0", &READING, at)?;
        assert!(matches!(
            slot.close(&store, &exchanged, &READING, at),
            Err(StoreError::Sqlite(_))
        ));
        assert_eq!(rows(&root, "answered")?, 0);
        Ok(())
    }
}
