use std::process::ExitCode;

use clap::Parser;
use hl_cli::{Cli, run};

fn main() -> ExitCode {
    run(Cli::parse())
}
