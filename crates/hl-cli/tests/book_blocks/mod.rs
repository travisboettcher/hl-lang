//! Scans `book/src/*.md` for ` ```hll `-tagged fenced blocks and hands
//! back one [`Block`] per fence, tags parsed.
//!
//! Shared by two integration tests rather than living in either of
//! them: `book_examples.rs` compiles every block, and
//! `compose_differential.rs` feeds the `build`-tagged ones to Docker
//! Compose's own parser. Duplicating the scan into the second test
//! would let the two drift — a new fence style, or a change to how an
//! info string is split, would silently stop reaching one of them, and
//! the one it stopped reaching would keep passing. Sharing it costs a
//! module; drifting costs coverage nobody notices is gone.
//!
//! Kept as a test-private module under `tests/` rather than promoted
//! into `hl-cli`'s own `src/`: nothing in the shipped compiler reads
//! the book, so this is test scaffolding, and putting it in `src/`
//! would add a public surface (and dead code in the binary) for a
//! reader that only tests have.

// Each integration-test target compiles this module separately, so an
// item only one of them uses reads as dead code in the other. The
// alternative — splitting the module along the seam of which test uses
// what — would defeat the point of sharing it.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// One ` ```hll `-fenced block lifted out of a book page.
pub struct Block {
    /// `book/src/<file>.md`, for error messages.
    pub source: String,
    /// 1-based line number of the fence's opening ` ``` `, for error
    /// messages.
    pub line: usize,
    pub code: String,
    pub attrs: Vec<String>,
}

impl Block {
    /// The value of an `attr=value` tag, e.g. `attr("file=")` for
    /// ` ```hll,file=network.hll `.
    pub fn attr<'a>(&'a self, prefix: &str) -> Option<&'a str> {
        self.attrs.iter().find_map(|a| a.strip_prefix(prefix))
    }

    /// Whether a bare tag such as `build` or `ignore` is present.
    pub fn has(&self, attr: &str) -> bool {
        self.attrs.iter().any(|a| a == attr)
    }

    /// `page.md:12`, the form every failure message in both tests uses
    /// so an editor can jump straight to the fence.
    pub fn location(&self) -> String {
        format!("{}:{}", self.source, self.line)
    }
}

pub fn book_src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../book/src")
}

/// Scans every `.md` file directly under `book/src` for ` ```hll `-tagged
/// fenced blocks. Deliberately not a general Markdown parser — a
/// hand-rolled line scan is enough for the book's own consistent
/// ` ```hll[,attr,...] ` / ` ``` ` fence style, and avoids a new
/// dev-dependency for a workspace with none today.
pub fn extract_blocks() -> Vec<Block> {
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
