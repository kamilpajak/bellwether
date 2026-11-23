//! `modelgate`: the only exit to a language model. Nothing in this crate opens a socket except
//! [`wire::ModelWire`], and its `send` consumes a [`ledger::SendPermit`] that only an
//! [`ledger::AttemptSlot`] with its `opened` row already in the store can hand out, so a model
//! call with no record of itself does not compile. What the type closes and what it leaves to a
//! manifest check: `docs/design/architecture.md` section 8 rows MG1 and R4.
//!
//! A send with no permit:
//!
//! ```compile_fail
//! use modelgate::wire::{ModelExchange, ModelWire};
//! # fn illegal(wire: &ModelWire, body: &[u8]) {
//! let _answer = wire.send(body); // E0061: an argument is missing
//! # }
//! ```
//!
//! **The positive control for the block above.** Same `use` line and the legal form, so a moved
//! path turns this block red while the one above would stay green (section 8 row R2).
//!
//! ```no_run
//! use modelgate::wire::{ModelExchange, ModelWire};
//! use modelgate::ledger::{AttemptSlot, Opening};
//! use modelgate::store::{Store, StoreError};
//! # fn legal(wire: &ModelWire, store: &Store, opening: &Opening, body: &[u8])
//! #     -> Result<(), StoreError> {
//! let mut slot = AttemptSlot::open(store, opening)?;
//! let permit = slot.take_permit()?;
//! let _answer = wire.send(permit, body);
//! # Ok(())
//! # }
//! ```

pub mod cap;
pub mod cmd_shadow;
pub mod ledger;
pub mod model;
pub mod question;
pub mod request;
pub mod response;
pub mod store;
pub mod wire;

#[cfg(test)]
mod tests {
    /// The two doc blocks in this file's header are the whole enforcement point of row MG1.
    const DOC_IMPORT: &str = "use modelgate::wire::{ModelExchange, ModelWire};";
    const COMPILE_FAIL: &str = "compile_fail";
    const NO_RUN: &str = "no_run";
    const SEND_WITHOUT_A_PERMIT: &str = "wire.send(body)";
    const SEND_WITH_A_PERMIT: &str = "wire.send(permit, body)";

    /// The header's fenced blocks, in order. The fence is assembled, so this function's own text
    /// does not split the file it reads.
    fn block(index: usize) -> String {
        let fence = format!("//! {}", "```");
        include_str!("lib.rs")
            .split(&fence)
            .skip(1)
            .step_by(2)
            .nth(index)
            .unwrap_or_default()
            .to_owned()
    }

    /// The fence's annotation EXACTLY: `compile_fail,ignore` starts with `compile_fail` and
    /// rustdoc does not run it (section 8 row R2).
    fn annotation(block: &str) -> &str {
        block.lines().next().unwrap_or_default()
    }

    #[test]
    fn a_send_without_a_permit_does_not_compile() {
        let illegal = block(0);
        assert_eq!(annotation(&illegal), COMPILE_FAIL, "{illegal}");
        assert!(illegal.contains(DOC_IMPORT), "{illegal}");
        assert!(illegal.contains(SEND_WITHOUT_A_PERMIT), "{illegal}");
    }

    #[test]
    fn the_positive_control_for_that_block_sends_with_a_permit_and_compiles() {
        let control = block(1);
        assert_eq!(annotation(&control), NO_RUN, "{control}");
        assert!(control.contains(DOC_IMPORT), "{control}");
        assert!(control.contains(SEND_WITH_A_PERMIT), "{control}");
    }
}
