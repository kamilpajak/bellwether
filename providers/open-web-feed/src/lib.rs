//! Our own forward news feed: dated indexes and article pages from admitted hosts, collected live
//! for the run's UTC day only. The design is `docs/design/architecture.md` section 11; this crate
//! is its build-order step 1 and holds nothing yet. It declares no HTTP dependency and holds no
//! clock and no `Sink`.
