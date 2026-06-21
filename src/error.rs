//! Crate-wide typed error returned by internal modules. Modules surface precise
//! [`Error`] variants; the binary boundary (`main`) layers human-facing context
//! on top via `anyhow`. New variants are added where a module first needs them.

use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum Error {
    #[error("failed to initialize logging: {0}")]
    Logging(String),

    #[error("failed to prepare state directory {path}: {source}")]
    StateDir {
        path: String,
        source: std::io::Error,
    },

    #[error("state database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("log I/O error: {0}")]
    Io(std::io::Error),
}

pub(crate) type Result<T> = std::result::Result<T, Error>;
