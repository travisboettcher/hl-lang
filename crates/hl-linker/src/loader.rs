//! Decouples the module graph ([`crate::graph`]) from where a file's
//! contents actually come from — the real filesystem ([`FsLoader`]) or
//! an in-memory map ([`InMemoryLoader`], used throughout this crate's
//! own tests so they don't need real files on disk). Path resolution
//! (joining a `use` decl's relative path against its importing file's
//! own directory, see [`crate::path`]) is graph-building logic, not the
//! loader's — a loader only ever answers "read me this already-resolved
//! path."

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

pub trait FileLoader {
    fn read(&self, path: &Path) -> io::Result<String>;
}

/// Reads real files off disk.
pub struct FsLoader;

impl FileLoader for FsLoader {
    fn read(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }
}

/// An in-memory [`FileLoader`] backed by a plain path→contents map — no
/// real filesystem needed, so a whole multi-file `use` graph can be
/// built and exercised in a test with no scratch directory at all.
#[derive(Default)]
pub struct InMemoryLoader {
    files: HashMap<PathBuf, String>,
}

impl InMemoryLoader {
    pub fn add(&mut self, path: impl Into<PathBuf>, contents: impl Into<String>) -> &mut Self {
        self.files.insert(path.into(), contents.into());
        self
    }
}

impl FileLoader for InMemoryLoader {
    fn read(&self, path: &Path) -> io::Result<String> {
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "file not found"))
    }
}
