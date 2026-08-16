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
#[command(name = "hllc", version, about)]
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
        let (hll_files, subdirs) = match scan_dir(path) {
            Ok(scanned) => scanned,
            Err(code) => return code,
        };

        // A directory holding `.hll` files directly is the flat case:
        // one independent entry point per file, `--out` required (there's
        // no single meaningful default location for potentially many
        // files' output). A directory holding no `.hll` files of its own
        // but at least one subdirectory that does is the co-located
        // case (#12): each such subdirectory's own `.hll` file(s) build
        // in place, right back into that same subdirectory by default.
        // A directory matching neither (no `.hll` files anywhere within
        // one level) builds nothing — same as today's flat case already
        // does when it finds zero `.hll` files.
        if !hll_files.is_empty() {
            return build_flat_directory(&hll_files, path, out);
        }
        if !subdirs.is_empty() {
            return build_colocated_directories(&subdirs, out);
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
            if let Err(code) = write_output(out_path, &yaml) {
                return code;
            }
            println!("{}", out_path.display());
        }
        None => print!("{yaml}"),
    }
    ExitCode::SUCCESS
}

/// Lists `dir`'s immediate children, split into `.hll` files and
/// subdirectories (both sorted, for deterministic build order).
fn scan_dir(dir: &Path) -> Result<(Vec<PathBuf>, Vec<PathBuf>), ExitCode> {
    let entries = fs::read_dir(dir).map_err(|err| {
        eprintln!("{}: {err}", dir.display());
        ExitCode::FAILURE
    })?;
    let mut hll_files = Vec::new();
    let mut subdirs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| {
            eprintln!("{}: {err}", dir.display());
            ExitCode::FAILURE
        })?;
        let entry_path = entry.path();
        if entry_path.extension().is_some_and(|ext| ext == "hll") {
            hll_files.push(entry_path);
        } else if entry_path.is_dir() {
            subdirs.push(entry_path);
        }
    }
    hll_files.sort();
    subdirs.sort();
    Ok((hll_files, subdirs))
}

/// The flat case: every `.hll` file directly inside `dir` is its own
/// independent entry point, written to `<out>/<stem>/docker-compose.yml`.
fn build_flat_directory(hll_files: &[PathBuf], dir: &Path, out: Option<&Path>) -> ExitCode {
    let Some(out_dir) = out else {
        eprintln!(
            "{}: --out <dir> is required when building a directory of .hll files",
            dir.display()
        );
        return ExitCode::FAILURE;
    };
    for hll_path in hll_files {
        let Some(stem) = hll_path.file_stem().and_then(|s| s.to_str()) else {
            eprintln!("{}: couldn't determine a file stem", hll_path.display());
            return ExitCode::FAILURE;
        };
        // `file_stem` is purely lexical and has no notion of a *usable*
        // directory name: a file literally called `...hll` yields the
        // stem `..`, and `out_dir.join("..")` resolves one level above
        // `--out`, writing output outside the directory the user chose
        // (#64). Anything that isn't a plain single path component is
        // rejected rather than normalized — there's no sensible output
        // directory such a name could mean.
        if stem.is_empty() || stem == "." || stem == ".." || stem.contains(std::path::is_separator)
        {
            eprintln!(
                "{}: `{stem}` isn't usable as an output directory name",
                hll_path.display()
            );
            return ExitCode::FAILURE;
        }
        if let Err(code) = build_one(hll_path, &out_dir.join(stem)) {
            return code;
        }
    }
    ExitCode::SUCCESS
}

/// The co-located case (#12): `subdirs` are `root`'s immediate child
/// directories, each expected to hold exactly one `.hll` file
/// co-located with its own `docker-compose.yml` (a real layout — see
/// docs/DESIGN.md's Pipeline section — where each service's `.hll`
/// source sits next to its other files, e.g. `.env`, bind-mounted
/// config, instead of every `.hll` file living in one flat directory).
/// Each subdirectory builds independently, writing to that same
/// subdirectory by default, or to `<out>/<subdir-name>/` if `--out` is
/// given — remapping the whole tree the same way flat mode's `--out`
/// already does, just keyed by directory name instead of file stem.
fn build_colocated_directories(subdirs: &[PathBuf], out: Option<&Path>) -> ExitCode {
    for subdir in subdirs {
        let (hll_files, _) = match scan_dir(subdir) {
            Ok(scanned) => scanned,
            Err(code) => return code,
        };
        if hll_files.is_empty() {
            continue;
        }
        if hll_files.len() > 1 {
            eprintln!(
                "{}: {} .hll files found, expected exactly one per co-located service directory",
                subdir.display(),
                hll_files.len()
            );
            return ExitCode::FAILURE;
        }
        let Some(name) = subdir.file_name().and_then(|s| s.to_str()) else {
            eprintln!("{}: couldn't determine a directory name", subdir.display());
            return ExitCode::FAILURE;
        };
        let target_dir = match out {
            Some(out_root) => out_root.join(name),
            None => subdir.clone(),
        };
        if let Err(code) = build_one(&hll_files[0], &target_dir) {
            return code;
        }
    }
    ExitCode::SUCCESS
}

/// Builds `hll_path`, writing the result to `target_dir/docker-compose.yml`
/// (creating `target_dir` if needed). Shared by both directory-scan
/// modes' per-entry-point loop.
fn build_one(hll_path: &Path, target_dir: &Path) -> Result<(), ExitCode> {
    let yaml = build_yaml(hll_path)?;
    fs::create_dir_all(target_dir).map_err(|err| {
        eprintln!("{}: {err}", target_dir.display());
        ExitCode::FAILURE
    })?;
    let out_path = target_dir.join("docker-compose.yml");
    write_output(&out_path, &yaml)?;
    println!("{}", out_path.display());
    Ok(())
}

/// Writes `yaml` to `out_path`, refusing to do so if `out_path` is a
/// symlink.
///
/// `fs::write` follows symlinks, so without this a `docker-compose.yml`
/// replaced by a link to some unrelated file would have *that* file
/// truncated and overwritten by a plain `hllc --build <repo>` — the
/// co-located mode in particular writes to paths it discovered by
/// scanning, not paths the user named (#64). Generated output only ever
/// lands on the path it's addressed to; replacing the link itself is
/// left to the user, who can see what it points at.
///
/// `symlink_metadata` is what makes the check meaningful — it reports on
/// the link rather than its target. Any other stat failure (most
/// commonly the file simply not existing yet, which is the normal case)
/// is deliberately ignored here and left for `fs::write` to report with
/// its own, more accurate error.
fn write_output(out_path: &Path, yaml: &str) -> Result<(), ExitCode> {
    if fs::symlink_metadata(out_path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        eprintln!(
            "{}: refusing to write through a symlink",
            out_path.display()
        );
        return Err(ExitCode::FAILURE);
    }
    fs::write(out_path, yaml).map_err(|err| {
        eprintln!("{}: {err}", out_path.display());
        ExitCode::FAILURE
    })
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
