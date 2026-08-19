//! Compiles every `hll` code example embedded in `book/src/*.md`, the
//! same way rustdoc's doctests exercise the code examples in Rust doc
//! comments — so a snippet that stops matching the actual grammar (a
//! rewording that quietly introduces invalid syntax, a field renamed in
//! the schema, ...) fails `cargo test` instead of silently rotting in
//! the published book.
//!
//! Fenced blocks are tagged via the info string, comma-separated after
//! the `hll` language tag (mirrors rustdoc's own `ignore`/`no_run`-style
//! fence attributes):
//!
//! - *(no attribute)* — a complete, syntactically standalone `.hll`
//!   file: parsed only, not built. Lets a snippet reference a network or
//!   template declared in an earlier, separate snippet (parsing doesn't
//!   resolve `with`/`networks` targets — only `compose`/`link` does),
//!   which most of the field-by-field examples in built-in-fields.md and
//!   templates-and-composition.md do deliberately, to stay focused on
//!   one field/concept at a time.
//! - `build` — parsed *and* fully built end-to-end (link -> compose ->
//!   codegen), for a complete, actually-deployable worked example.
//! - `fragment` — one or more bare statements, not a valid top-level
//!   file on its own (e.g. `image "foo"` with no enclosing `service`);
//!   wrapped in `service __book_example__ { ... }` before parsing.
//! - `file=NAME,group=ID[,entry]` — one file of a multi-file `use`
//!   example. Every block sharing `group=ID` is loaded into one
//!   `InMemoryLoader`, keyed by its own `file=NAME`; the block also
//!   marked `entry` is the one `link`+`generate` runs against.
//! - `ignore` — excluded from validation entirely.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

struct Block {
    /// `book/src/<file>.md`, for error messages.
    source: String,
    /// 1-based line number of the fence's opening ` ``` `, for error
    /// messages.
    line: usize,
    code: String,
    attrs: Vec<String>,
}

impl Block {
    fn attr<'a>(&'a self, prefix: &str) -> Option<&'a str> {
        self.attrs.iter().find_map(|a| a.strip_prefix(prefix))
    }

    fn has(&self, attr: &str) -> bool {
        self.attrs.iter().any(|a| a == attr)
    }

    fn location(&self) -> String {
        format!("{}:{}", self.source, self.line)
    }
}

fn book_src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../book/src")
}

/// Scans every `.md` file directly under `book/src` for ` ```hll `-tagged
/// fenced blocks. Deliberately not a general Markdown parser — a
/// hand-rolled line scan is enough for the book's own consistent
/// ` ```hll[,attr,...] ` / ` ``` ` fence style, and avoids a new
/// dev-dependency for a workspace with none today.
fn extract_blocks() -> Vec<Block> {
    let dir = book_src_dir();
    let mut md_files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("reading {}: {err}", dir.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    md_files.sort();

    let mut blocks = Vec::new();
    for path in md_files {
        let source_name = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));

        let mut lines = text.lines().enumerate().peekable();
        while let Some((idx, line)) = lines.next() {
            let Some(info) = line.strip_prefix("```hll") else {
                continue;
            };
            let attrs: Vec<String> = info
                .trim_start_matches(',')
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();

            let mut code = String::new();
            for (_, body_line) in lines.by_ref() {
                if body_line == "```" {
                    break;
                }
                code.push_str(body_line);
                code.push('\n');
            }

            blocks.push(Block {
                source: source_name.clone(),
                line: idx + 1,
                code,
                attrs,
            });
        }
    }
    blocks
}

fn parse_ok(src: &str) -> Result<(), String> {
    hl_parser::parse(src)
        .map(|_| ())
        .map_err(|err| err.to_string())
}

fn build_ok(src: &str) -> Result<(), String> {
    let mut loader = hl_linker::InMemoryLoader::default();
    loader.add("example.hll", src);
    let linked =
        hl_linker::link(Path::new("example.hll"), &loader).map_err(|err| err.to_string())?;
    hl_codegen::generate(linked.program)
        .map(|_| ())
        .map_err(|err| err.to_string())
}

#[test]
fn book_examples_compile() {
    let blocks = extract_blocks();
    assert!(
        !blocks.is_empty(),
        "found zero ```hll blocks under {} -- extraction is probably broken",
        book_src_dir().display()
    );

    let mut failures = Vec::new();
    let mut groups: HashMap<String, Vec<&Block>> = HashMap::new();

    for block in &blocks {
        if block.has("ignore") {
            continue;
        }
        if let Some(group) = block.attr("group=") {
            groups.entry(group.to_string()).or_default().push(block);
            continue;
        }

        let src = if block.has("fragment") {
            format!("service __book_example__ {{\n{}}}\n", block.code)
        } else {
            block.code.clone()
        };

        if let Err(err) = parse_ok(&src) {
            failures.push(format!("{}: parse error: {err}", block.location()));
            continue;
        }
        if block.has("build")
            && let Err(err) = build_ok(&src)
        {
            failures.push(format!("{}: build error: {err}", block.location()));
        }
    }

    for (group, members) in groups {
        let mut loader = hl_linker::InMemoryLoader::default();
        let mut entry = None;
        for block in &members {
            let Some(name) = block.attr("file=") else {
                failures.push(format!(
                    "{}: group `{group}` block has no `file=NAME` attribute",
                    block.location()
                ));
                continue;
            };
            loader.add(name, &block.code);
            if block.has("entry") {
                entry = Some((name.to_string(), block.location()));
            }
        }
        let Some((entry_name, entry_loc)) = entry else {
            failures.push(format!("group `{group}` has no block tagged `entry`"));
            continue;
        };
        match hl_linker::link(Path::new(&entry_name), &loader) {
            Ok(linked) => {
                if let Err(err) = hl_codegen::generate(linked.program) {
                    failures.push(format!("{entry_loc} (group `{group}`): build error: {err}"));
                }
            }
            Err(err) => failures.push(format!("{entry_loc} (group `{group}`): link error: {err}")),
        }
    }

    assert!(
        failures.is_empty(),
        "\n{} book example(s) failed to compile:\n{}\n",
        failures.len(),
        failures.join("\n")
    );
}
