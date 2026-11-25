//! `--pace-root`, in a module of its own so its field is private: only `clap` builds a [`Pace`],
//! and no command can resolve one it wrote by hand (section 8 row R19, hole 2).

use std::io::Write;
use std::path::{Path, PathBuf};

use news_provider::{PaceRoot, PaceRootError};

use crate::Format;

/// The option of every command that builds a `Wire` (architecture.md section 12.2).
#[derive(clap::Args)]
pub struct Pace {
    /// The existing absolute directory whose pace/ holds this deployable's pace files, shared by
    /// every command that sends. Defaults to the store. Refused while it is not the store and the
    /// store's own pace/ still holds a file.
    #[arg(long)]
    pace_root: Option<PathBuf>,
}

impl Pace {
    /// A refusal is written to stderr here, stdout left empty, and is the exit.
    pub fn resolve(&self, store: &Path, format: Format) -> PaceRoot {
        let refused = match PaceRoot::resolve(self.pace_root.as_deref(), store) {
            Ok(root) => return root,
            Err(refused) => refused,
        };
        let object = serde_json::json!({
            "schema": "newsvault.error/1",
            "code": refused.code(),
            "message": refused.to_string(),
            "retryable": false,
            "details": {
                "store": store.display().to_string(),
                "pace_root": self.pace_root.as_ref().map(|root| root.display().to_string()),
                "files": match &refused {
                    PaceRootError::Split { files, .. } | PaceRootError::Nested { files, .. } => {
                        files.clone()
                    }
                    _ => Vec::new(),
                },
            },
            "suggestions": [],
        });
        #[allow(
            clippy::let_underscore_must_use,
            reason = "reporting a refusal must not fail on a closed stderr; the exit code is the verdict"
        )]
        let _ = match format {
            Format::Json => writeln!(std::io::stderr(), "{object}"),
            Format::Human => writeln!(std::io::stderr(), "error: {refused} ({})", refused.code()),
        };
        std::process::exit(i32::from(refused.exit()));
    }

    /// The option as given, for an argv that suggests another sending command.
    pub fn argv(&self) -> Vec<String> {
        self.pace_root
            .iter()
            .flat_map(|root| ["--pace-root".to_owned(), root.display().to_string()])
            .collect()
    }
}
