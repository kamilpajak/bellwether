//! The filings context (architecture.md section 12): its store, its ledgered conversation with
//! SEC, discovery by the daily form index and the coverage ledger over index days. It reaches no
//! sibling context (section 8 row FV1) and declares no HTTP client (row R4): every request leaves
//! through `news_provider::Wire` under an attempt row this crate's sink wrote first.

pub mod cli;
pub mod collect;
pub mod discover;
pub mod error;
pub mod ledger;
pub mod pace_option;
pub mod rules;
pub mod store;
