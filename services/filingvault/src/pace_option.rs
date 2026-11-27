//! `--pace-root`, in a module of its own so its field is private: only `clap` builds a [`Pace`]
//! (architecture.md section 8 row R19, hole 2). Required here: `filingvault` has no store-side
//! pace directory to fall back to (section 12.2).

use std::path::{Path, PathBuf};

use news_provider::{PaceRoot, PaceRootError};

#[derive(clap::Args, Debug)]
pub struct Pace {
    /// The existing absolute directory whose pace/ holds this deployable's pace files, shared by
    /// every command that sends to SEC.
    #[arg(long, required = true)]
    pace_root: Option<PathBuf>,
}

impl Pace {
    pub fn resolve(&self, store: &Path) -> Result<PaceRoot, PaceRootError> {
        PaceRoot::resolve(self.pace_root.as_deref(), store)
    }
}
