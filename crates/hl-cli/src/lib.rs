//! Library backing the `hl-cli` binary, kept separate from `main.rs` so
//! the CLI's actual logic is testable and growable (more subcommands and
//! options are expected once the parser/codegen stages exist) independent
//! of process-level concerns like `std::env::args` and exit codes.

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use hl_lexer::Lexer;

/// Lex (or, with `--parse`, parse) an hl-lang (`.hll`) source file and
/// print the result.
#[derive(Debug, Parser)]
#[command(name = "hl-cli", version, about)]
pub struct Cli {
    /// Path to an .hll source file.
    pub file: PathBuf,
    /// Parse the file and pretty-print its AST instead of just lexing.
    #[arg(long)]
    pub parse: bool,
}

/// Runs the CLI for an already-parsed [`Cli`] invocation.
pub fn run(cli: Cli) -> ExitCode {
    let path = &cli.file;

    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(err) => {
            eprintln!("{}: {err}", path.display());
            return ExitCode::FAILURE;
        }
    };

    if cli.parse {
        return match hl_parser::parse(&source) {
            Ok(program) => {
                println!("{program:#?}");
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("{}: {err}", path.display());
                ExitCode::FAILURE
            }
        };
    }

    for result in Lexer::new(&source) {
        match result {
            Ok(token) => {
                println!(
                    "{}:{} {:?} {:?}",
                    token.span.line, token.span.col, token.kind, token.lexeme
                );
            }
            Err(err) => {
                eprintln!("{}:{err}", path.display());
                return ExitCode::FAILURE;
            }
        }
    }

    ExitCode::SUCCESS
}
