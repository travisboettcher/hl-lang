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
//!
//! A `build` block usually prints the document it produces underneath
//! itself, and `book_documented_output_matches` holds those to the
//! compiler. Compiling an example only proves it still *works*; the
//! YAML beside it is a separate claim about what it produces, and
//! nothing checked that claim until #271 left three of them describing
//! labels `hllc` had stopped generating.

// The fence scanner itself lives in `book_blocks/` because
// `compose_differential.rs` needs the same blocks — see that module's
// own doc for why it isn't duplicated. The tag list above stays here,
// with the test that gives each tag its meaning.
mod book_blocks;

use std::collections::HashMap;
use std::path::Path;

use book_blocks::{Block, book_src_dir, extract_blocks};
use serde_yaml_ng::Value;

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

/// Builds one source the way `book_examples_compile` does, and hands
/// back the generated document rather than discarding it.
fn build_output(src: &str) -> Result<String, String> {
    let mut loader = hl_linker::InMemoryLoader::default();
    loader.add("example.hll", src);
    let linked =
        hl_linker::link(Path::new("example.hll"), &loader).map_err(|err| err.to_string())?;
    hl_codegen::generate(linked.program)
        .map(|generated| generated.yaml)
        .map_err(|err| err.to_string())
}

/// Whether `expected` describes some part of `actual`.
///
/// Equality is the ordinary case: a page that prints a whole document
/// gets that document checked whole. The looser arm is for the pages
/// that print an excerpt — just the `labels:` a section is about, say —
/// where the surrounding service would be noise. Such an excerpt matches
/// a mapping that carries every key it names, **with exactly the value
/// it names**, which is what keeps "excerpt" from meaning "unchecked":
/// a documented `labels:` list still has to be the list `hllc` emits,
/// entry for entry.
fn describes(expected: &Value, actual: &Value) -> bool {
    if expected == actual {
        return true;
    }
    if let (Value::Mapping(want), Value::Mapping(have)) = (expected, actual)
        && want.iter().all(|(k, v)| have.get(k) == Some(v))
    {
        return true;
    }
    match actual {
        Value::Mapping(m) => m.values().any(|v| describes(expected, v)),
        Value::Sequence(s) => s.iter().any(|v| describes(expected, v)),
        _ => false,
    }
}

/// Every `build` example that prints its output generates exactly that.
///
/// The gap this closes: `book_examples_compile` runs the compiler over
/// each example and throws the result away, so an example goes on
/// passing while the YAML printed beneath it drifts into fiction. That
/// is not hypothetical — #271 stopped the compiler generating
/// `traefik.docker.network`, and three blocks went on claiming it,
/// one of them on an example about *Caddy*. Nothing failed, because
/// nothing was looking.
///
/// Compared as parsed YAML rather than as text, so the check is about
/// what the document *says*. Sequence indentation and key order are the
/// emitter's business and change nothing a reader relies on; a label
/// that isn't there is a different document.
#[test]
fn book_documented_output_matches() {
    let mut failures = Vec::new();
    let mut checked = 0;

    for block in extract_blocks() {
        // `file=` blocks are one file of a multi-file group, built
        // together by the test above; the group's output isn't this
        // block's to claim.
        let (Some(expected_src), true, None) = (
            block.expected_output.as_deref(),
            block.has("build"),
            block.attr("file="),
        ) else {
            continue;
        };

        let actual_src = match build_output(&block.code) {
            Ok(yaml) => yaml,
            // A `build` block that no longer builds is the other test's
            // failure to report, not this one's — saying it twice would
            // make one broken example look like two.
            Err(_) => continue,
        };

        let expected: Value = match serde_yaml_ng::from_str(expected_src) {
            Ok(value) => value,
            Err(err) => {
                failures.push(format!(
                    "{}: the block beneath this example is not YAML ({err}) — a `build` \
                     example's output block documents a generated document, so if this one \
                     is prose it wants a blank line between it and the example",
                    block.location()
                ));
                continue;
            }
        };
        let actual: Value = serde_yaml_ng::from_str(&actual_src)
            .unwrap_or_else(|err| panic!("hllc emitted invalid YAML: {err}\n{actual_src}"));

        checked += 1;
        if !describes(&expected, &actual) {
            failures.push(format!(
                "{}: the output documented here is not what this example generates\n\
                 --- documented ---\n{expected_src}--- actual ---\n{actual_src}",
                block.location()
            ));
        }
    }

    assert!(
        checked >= 20,
        "expected the book to document output for at least 20 `build` examples, found \
         {checked} — if output blocks were deliberately removed, lower this floor; \
         otherwise the extractor has stopped finding them and this test is passing \
         vacuously"
    );
    assert!(
        failures.is_empty(),
        "{} documented output block(s) disagree with the compiler:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
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
