//! `filingvault`: the command line over [`filingvault::cli`].

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let cli = filingvault::cli::Cli::parse();
    let argv: Vec<String> = std::env::args().collect();
    ExitCode::from(filingvault::cli::run(
        cli,
        &argv,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    ))
}
