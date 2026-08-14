//! Library backing the `hl-cli` binary, kept separate from `main.rs` so
//! the CLI's actual logic is testable and growable (more subcommands and
//! options are expected once the parser/codegen stages exist) independent
//! of process-level concerns like `std::env::args` and exit codes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use hl_lexer::Lexer;

/// Lex, parse (`--parse`), or build Compose YAML (`--build`) from an
/// hl-lang (`.hll`) source file or, in `--build`'s case, a directory of
/// them.
#[derive(Debug, Parser)]
#[command(name = "hl-cli", version, about)]
pub struct Cli {
    /// Path to an .hll source file, or (with `--build`) a directory of them.
    pub file: PathBuf,
    /// Parse the file and pretty-print its AST instead of just lexing.
    #[arg(long)]
    pub parse: bool,
    /// Compile the file (or every `.hll` file directly inside the
    /// directory) into Compose YAML: parse, resolve template/`with`
    /// composition, then generate. One input file always produces one
    /// output document (it may hold multiple services).
    #[arg(long)]
    pub build: bool,
    /// Where to write `--build`'s output. For a single input file,
    /// omitting this prints the YAML to stdout; for a directory input,
    /// this is required (each input file's stem becomes
    /// `<out>/<stem>/docker-compose.yml`).
    #[arg(long)]
    pub out: Option<PathBuf>,
}

/// Runs the CLI for an already-parsed [`Cli`] invocation.
pub fn run(cli: Cli) -> ExitCode {
    if cli.build {
        return run_build(&cli.file, cli.out.as_deref());
    }

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

fn run_build(path: &Path, out: Option<&Path>) -> ExitCode {
    let is_dir = match fs::metadata(path) {
        Ok(meta) => meta.is_dir(),
        Err(err) => {
            eprintln!("{}: {err}", path.display());
            return ExitCode::FAILURE;
        }
    };

    if is_dir {
        let Some(out_dir) = out else {
            eprintln!(
                "{}: --out <dir> is required when building a directory of .hll files",
                path.display()
            );
            return ExitCode::FAILURE;
        };
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(err) => {
                eprintln!("{}: {err}", path.display());
                return ExitCode::FAILURE;
            }
        };
        let mut hll_files: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    eprintln!("{}: {err}", path.display());
                    return ExitCode::FAILURE;
                }
            };
            let entry_path = entry.path();
            if entry_path.extension().is_some_and(|ext| ext == "hll") {
                hll_files.push(entry_path);
            }
        }
        hll_files.sort();

        for hll_path in hll_files {
            let Some(stem) = hll_path.file_stem().and_then(|s| s.to_str()) else {
                eprintln!("{}: couldn't determine a file stem", hll_path.display());
                return ExitCode::FAILURE;
            };
            let yaml = match build_yaml(&hll_path) {
                Ok(yaml) => yaml,
                Err(code) => return code,
            };
            let service_dir = out_dir.join(stem);
            if let Err(err) = fs::create_dir_all(&service_dir) {
                eprintln!("{}: {err}", service_dir.display());
                return ExitCode::FAILURE;
            }
            let out_path = service_dir.join("docker-compose.yml");
            if let Err(err) = fs::write(&out_path, &yaml) {
                eprintln!("{}: {err}", out_path.display());
                return ExitCode::FAILURE;
            }
            println!("{}", out_path.display());
        }
        return ExitCode::SUCCESS;
    }

    let yaml = match build_yaml(path) {
        Ok(yaml) => yaml,
        Err(code) => return code,
    };
    match out {
        Some(out_path) => {
            if let Some(parent) = out_path.parent()
                && !parent.as_os_str().is_empty()
                && let Err(err) = fs::create_dir_all(parent)
            {
                eprintln!("{}: {err}", parent.display());
                return ExitCode::FAILURE;
            }
            if let Err(err) = fs::write(out_path, &yaml) {
                eprintln!("{}: {err}", out_path.display());
                return ExitCode::FAILURE;
            }
            println!("{}", out_path.display());
        }
        None => print!("{yaml}"),
    }
    ExitCode::SUCCESS
}

/// Loads `path`'s whole `use` graph and generates Compose YAML for it.
///
/// Errors are printed bare (`eprintln!("{err}")`), not wrapped with
/// `path`'s own display like every other error site in this file:
/// `hl_linker::LinkError`'s `Io`/`Parse`/`DuplicateAlias` variants
/// already self-prefix with *their own* correct path, which may be an
/// imported file rather than `path` itself, and both `LinkError::Compose`
/// and `hl_codegen::CodegenError` only ever carry a `{line}:{col}`
/// location with no file — accurate for a single file, but a composed
/// service's fields can now originate from any file in the graph, so
/// guessing `path` here would misattribute an error that actually came
/// from an imported template.
fn build_yaml(path: &Path) -> Result<String, ExitCode> {
    let composed = hl_linker::link(path, &hl_linker::FsLoader).map_err(|err| {
        eprintln!("{err}");
        ExitCode::FAILURE
    })?;
    let generated = hl_codegen::generate(composed).map_err(|err| {
        eprintln!("{err}");
        ExitCode::FAILURE
    })?;
    Ok(generated.yaml)
}
