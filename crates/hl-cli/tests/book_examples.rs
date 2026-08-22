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

// The fence scanner itself lives in `book_blocks/` because
// `compose_differential.rs` needs the same blocks — see that module's
// own doc for why it isn't duplicated. The tag list above stays here,
// with the test that gives each tag its meaning.
mod book_blocks;

use std::collections::HashMap;
use std::path::Path;

use book_blocks::{Block, book_src_dir, extract_blocks};

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
