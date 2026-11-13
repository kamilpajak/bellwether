//! The provider seam. Nothing in this workspace opens a socket except [`Wire::send`], and
//! `send` consumes a [`SendPermit`] that only an [`AttemptSlot`] with its `Opened` row already in
//! a [`Sink`] can hand out, so a fetch with no record of itself does not compile. What the type
//! closes and what it leaves to a manifest check: `docs/design/architecture.md` section 8 row R4.
//!
//! A send with no permit:
//!
//! ```compile_fail
//! use news_provider::{Clock, Request, Wire};
//! # fn illegal(wire: &Wire, request: &Request, clock: &dyn Clock) {
//! let _answer = wire.send(request, clock); // E0061: an argument is missing
//! # }
//! ```
//!
//! **The positive control for the block above.** A `compile_fail` block passes whenever its
//! snippet stops compiling for *any* reason, so on its own it tests "this does not compile" and
//! not "a send needs a permit". This block carries the same `use` line and the legal form, so a
//! moved path turns it red while the block above would stay green. Same pairing, same reason, as
//! `pit`'s two time types - architecture.md section 8 row R2.
//!
//! ```no_run
//! use news_provider::{Clock, Request, Wire};
//! use news_provider::{AttemptEvent, AttemptSlot, LedgerError, RunId, Sink};
//! # use pit::ObservationTime;
//! struct Nowhere;
//! impl Sink for Nowhere {
//!     fn write(&mut self, _event: &AttemptEvent) -> Result<(), LedgerError> { Ok(()) }
//! }
//!
//! # fn legal(wire: &Wire, request: &Request, clock: &dyn Clock, run: &RunId,
//! #          at: ObservationTime) -> Result<(), LedgerError> {
//! let mut sink = Nowhere;
//! let mut slot = AttemptSlot::open(&mut sink, request, run, 0, at)?;
//! let permit = slot.take_permit()?;
//! let _answer = wire.send(permit, request, clock);
//! # Ok(())
//! # }
//! ```

pub mod clock;
pub mod ledger;
pub mod request;
pub mod wire;

pub use clock::{Clock, SystemClock};
pub use ledger::{
    abandon, AttemptEvent, AttemptId, AttemptReceipt, AttemptSlot, Baseline, Cost, Counts,
    LedgerError, Outcome, OutcomeKind, SendPermit, Sink, Verdict,
};
pub use request::{
    CredentialRef, HeaderName, HeaderValue, Param, Request, RunId, SourceId, TargetWindow,
};
pub use wire::{
    Credentials, RawResponse, SendFailure, Timeouts, TransportError, TransportProfile, Wire,
};

#[cfg(test)]
mod tests {
    /// The two doc blocks in this file's header are the whole enforcement point of the capability.
    const DOC_IMPORT: &str = "use news_provider::{Clock, Request, Wire};";
    const COMPILE_FAIL: &str = "compile_fail";
    const NO_RUN: &str = "no_run";
    const SEND_WITHOUT_A_PERMIT: &str = "wire.send(request, clock)";
    const SEND_WITH_A_PERMIT: &str = "wire.send(permit, request, clock)";
    const ILLEGAL_BLOCK: usize = 0;
    const CONTROL_BLOCK: usize = 1;

    /// The header's fenced blocks, in order. The fence is assembled rather than written out, so
    /// this function's own text does not split the file it reads.
    fn doc_blocks() -> Vec<String> {
        let fence = format!("//! {}", "```");
        include_str!("lib.rs")
            .split(&fence)
            .skip(1)
            .step_by(2)
            .map(str::to_owned)
            .collect()
    }

    fn block(index: usize) -> String {
        doc_blocks().get(index).cloned().unwrap_or_default()
    }

    /// The fence's annotation EXACTLY, not a prefix of it. `compile_fail,ignore` starts with
    /// `compile_fail` and rustdoc does not run it: measured 2026-11-13, adding `,ignore` to the
    /// first fence and replacing the second with `ignore` left `cargo test` at exit 0 with 170
    /// test lines, both guards below green, and the call clause of §8 row R4 enforcing nothing.
    fn annotation(block: &str) -> &str {
        block.lines().next().unwrap_or_default()
    }

    #[test]
    fn a_send_without_a_permit_does_not_compile() {
        let illegal = block(ILLEGAL_BLOCK);
        assert_eq!(annotation(&illegal), COMPILE_FAIL, "{illegal}");
        assert!(illegal.contains(DOC_IMPORT), "{illegal}");
        assert!(illegal.contains(SEND_WITHOUT_A_PERMIT), "{illegal}");
    }

    #[test]
    fn the_positive_control_for_that_block_sends_with_a_permit_and_compiles() {
        let control = block(CONTROL_BLOCK);
        assert_eq!(annotation(&control), NO_RUN, "{control}");
        assert!(control.contains(DOC_IMPORT), "{control}");
        assert!(control.contains(SEND_WITH_A_PERMIT), "{control}");
    }
}
