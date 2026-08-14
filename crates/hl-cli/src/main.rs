use std::env;
use std::fs;
use std::process::ExitCode;

use hl_lexer::Lexer;

fn main() -> ExitCode {
    let mut args = env::args();
    let program = args.next().unwrap_or_else(|| "hl-cli".to_string());
    let Some(path) = args.next() else {
        eprintln!("usage: {program} <file.dsl>");
        return ExitCode::FAILURE;
    };

    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(err) => {
            eprintln!("{path}: {err}");
            return ExitCode::FAILURE;
        }
    };

    for result in Lexer::new(&source) {
        match result {
            Ok(token) => {
                println!(
                    "{}:{} {:?} {:?}",
                    token.span.line, token.span.col, token.kind, token.lexeme
                );
            }
            Err(err) => {
                eprintln!("{path}:{err}");
                return ExitCode::FAILURE;
            }
        }
    }

    ExitCode::SUCCESS
}
