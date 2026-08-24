use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Which source file a [`Span`] came from.
///
/// An opaque index into a [`SourceMap`], not a path: a `Span` is `Copy`
/// and lives in every AST node and every diagnostic, so it can't afford
/// to own (or borrow) one. The map that resolves the index back to a
/// path is built once, while the files are being read — see
/// `hl_linker`'s module graph, which interns each module's path as it
/// loads it and hands the finished map to the composed program and to
/// any error that escapes.
///
/// Spans that were never attributed to a file — anything lexed through
/// [`crate::Lexer::new`] or parsed through `hl_parser::parse`, i.e. the
/// single-file APIs that are handed source text and nothing else —
/// carry [`FileId::ANONYMOUS`], which resolves to no path and renders
/// as a bare `line:col`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(u32);

impl FileId {
    /// The identity of a span belonging to no known file.
    ///
    /// This is `FileId`'s [`Default`], so the single-file entry points
    /// don't have to name it, and [`SourceMap::path`] deliberately
    /// returns `None` for it rather than some placeholder path — a
    /// location with no file renders as `line:col`, exactly as it did
    /// before file identity existed.
    pub const ANONYMOUS: FileId = FileId(0);

    /// Whether this is [`FileId::ANONYMOUS`] — i.e. no [`SourceMap`] can
    /// resolve it to a path.
    pub fn is_anonymous(self) -> bool {
        self == FileId::ANONYMOUS
    }
}

/// The table that resolves a [`FileId`] back to the path it was interned
/// from.
///
/// Deliberately tiny and cheap to clone: the finished map is attached to
/// the composed program *and* to any error that escapes the linker, and
/// it is one word wide so those errors stay small enough to keep
/// returning by value (see [`Span`]'s "Why `u32` offsets"). The `Arc` is
/// only ever written through [`SourceMap::intern`] while the graph is
/// still being built, when it is uniquely owned, so copy-on-write never
/// actually copies in practice.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceMap {
    /// Indexed by `FileId(i)` at `paths[i - 1]`; index 0 is reserved for
    /// [`FileId::ANONYMOUS`], which is why the stored ids are 1-based.
    paths: Arc<Vec<PathBuf>>,
}

impl SourceMap {
    /// Returns the [`FileId`] for `path`, adding it to the map if it
    /// isn't there yet. Interning the same path twice returns the same
    /// id, so callers don't have to memoize themselves.
    pub fn intern(&mut self, path: impl Into<PathBuf>) -> FileId {
        let path = path.into();
        if let Some(index) = self.paths.iter().position(|known| *known == path) {
            return FileId(index as u32 + 1);
        }
        Arc::make_mut(&mut self.paths).push(path);
        FileId(self.paths.len() as u32)
    }

    /// The path `file` was interned from, or `None` for
    /// [`FileId::ANONYMOUS`] or an id from a different map.
    pub fn path(&self, file: FileId) -> Option<&Path> {
        let index = (file.0 as usize).checked_sub(1)?;
        self.paths.get(index).map(PathBuf::as_path)
    }

    /// How many distinct files have been interned.
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    /// Whether no file has been interned yet.
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

/// A span's position, ready to render in a diagnostic: `path:line:col`
/// when the file is known, plain `line:col` when it isn't.
///
/// Produced by [`Span::locate`]. Every diagnostic in the workspace
/// formats its positions through this one type, so a location gains its
/// file the moment the span carries one — no per-error-variant
/// formatting to keep in sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location<'a> {
    path: Option<&'a Path>,
    line: u32,
    col: u32,
}

impl<'a> Location<'a> {
    /// The file this location is in, if it's known.
    pub fn path(&self) -> Option<&'a Path> {
        self.path
    }

    /// The 1-indexed line.
    pub fn line(&self) -> u32 {
        self.line
    }

    /// The 1-indexed column, counted in `char`s (see [`Span`]).
    pub fn col(&self) -> u32 {
        self.col
    }
}

impl fmt::Display for Location<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.path {
            Some(path) => write!(f, "{}:{}:{}", path.display(), self.line, self.col),
            None => write!(f, "{}:{}", self.line, self.col),
        }
    }
}

/// Location of a token in the source text.
///
/// `start`/`end` are 0-indexed byte offsets (end-exclusive) into the
/// original source string. `line`/`col` are 1-indexed and describe the
/// position of `start`; `col` counts Unicode scalar values (`char`s), not
/// bytes or visual width — tabs are not expanded to a tab stop. `file`
/// identifies which source the offsets are into (see [`FileId`]); it is
/// [`FileId::ANONYMOUS`] unless the source was lexed through
/// [`crate::Lexer::new_in_file`].
///
/// For a [`crate::TokenKind::Str`] token, `span` covers the entire token
/// including both quote characters, while the token's `lexeme` is the
/// quote-stripped inner content — so for string tokens
/// `span.end - span.start == lexeme.len() + 2`. That holds for a
/// literal carrying backslash escapes too: the `lexeme` is source text,
/// not the shorter decoded value, so its bytes still line up with the
/// bytes the span covers.
///
/// The `Eof` token's span is zero-width (`start == end`) at `source.len()`.
///
/// # Why `u32` offsets
///
/// A `Span` is copied into every AST node and every diagnostic, and it
/// is what makes each error enum as big as it is — `Result`s carrying
/// those enums are returned all through the pipeline, and clippy's
/// `result_large_err` lint keeps a ceiling on that. `u32` offsets (the
/// same choice rustc's own `BytePos` makes) leave room for the [`FileId`]
/// added in #75 while keeping `Span` *smaller* than it was with `usize`
/// offsets and no file. The cost is a 4 GiB ceiling on a single source
/// file, which no `.hll` file will approach; past it, the lexer saturates
/// rather than wrapping, so offsets degrade instead of pointing
/// somewhere wrong.
///
/// Deliberately *not* `Default`: a span with no position is not a
/// meaningful value, and making one available would let a mutated
/// `fn span(&self) -> Span` fabricate one (`cargo mutants` in CI treats
/// that as an escaped mutant).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
    pub line: u32,
    pub col: u32,
    pub file: FileId,
}

impl Span {
    /// Renders this span's position for a diagnostic, resolving its
    /// [`FileId`] against `files`.
    ///
    /// `files` is an `Option` because the same diagnostic types are
    /// reachable both from the multi-file pipeline (which has a map) and
    /// from the single-file `hl_parser::parse`/`hl_parser::compose`
    /// entry points (which don't). Passing `None` — or a map that
    /// doesn't know this span's file — yields a bare `line:col`.
    pub fn locate<'a>(self, files: Option<&'a SourceMap>) -> Location<'a> {
        Location {
            path: files.and_then(|files| files.path(self.file)),
            line: self.line,
            col: self.col,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(line: u32, col: u32, file: FileId) -> Span {
        Span {
            start: 0,
            end: 0,
            line,
            col,
            file,
        }
    }

    #[test]
    fn anonymous_is_the_default_and_resolves_to_nothing() {
        assert_eq!(FileId::default(), FileId::ANONYMOUS);
        assert!(FileId::ANONYMOUS.is_anonymous());
        let mut files = SourceMap::default();
        assert!(files.is_empty());
        let real = files.intern("a.hll");
        assert!(!real.is_anonymous());
        assert!(!files.is_empty());
        assert_eq!(files.path(FileId::ANONYMOUS), None);
    }

    #[test]
    fn interning_is_stable_and_deduplicating() {
        let mut files = SourceMap::default();
        let a = files.intern("a.hll");
        let b = files.intern(PathBuf::from("dir/b.hll"));
        assert_eq!(files.intern("a.hll"), a);
        assert_ne!(a, b);
        assert_eq!(files.len(), 2);
        assert_eq!(files.path(a), Some(Path::new("a.hll")));
        assert_eq!(files.path(b), Some(Path::new("dir/b.hll")));
    }

    #[test]
    fn locate_renders_path_line_col_when_the_file_is_known() {
        let mut files = SourceMap::default();
        let a = files.intern("shared/base.hll");
        assert_eq!(
            span(2, 11, a).locate(Some(&files)).to_string(),
            "shared/base.hll:2:11"
        );
    }

    #[test]
    fn locate_falls_back_to_bare_line_col() {
        let files = SourceMap::default();
        let anon = span(2, 11, FileId::ANONYMOUS);
        assert_eq!(anon.locate(None).to_string(), "2:11");
        assert_eq!(anon.locate(Some(&files)).to_string(), "2:11");
        // An id from some other map resolves to nothing here, rather
        // than to whatever happens to sit at that index.
        assert_eq!(
            span(2, 11, FileId(7)).locate(Some(&files)).to_string(),
            "2:11"
        );
    }

    #[test]
    fn location_exposes_its_parts() {
        let mut files = SourceMap::default();
        let a = files.intern("a.hll");
        let loc = span(3, 4, a).locate(Some(&files));
        assert_eq!(loc.path(), Some(Path::new("a.hll")));
        assert_eq!(loc.line(), 3);
        assert_eq!(loc.col(), 4);
    }
}
