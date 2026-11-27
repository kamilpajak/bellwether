//! `filingvault`: the command line over [`filingvault::cli`].

use std::process::ExitCode;

use clap::Parser;
use filingvault::cli::{refused, run, Cli};

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let (out, err) = (&mut std::io::stdout(), &mut std::io::stderr());
    ExitCode::from(match Cli::try_parse_from(&argv) {
        Ok(cli) => run(cli, &argv, out, err),
        Err(why) => refused(&why, &argv, out, err),
    })
}
