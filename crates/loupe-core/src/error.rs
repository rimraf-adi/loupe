use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("index at {0} is locked by another writer")]
    Locked(PathBuf),

    #[error("corrupt index data: {0}")]
    Corrupt(String),

    #[error("unsupported on-disk format version {found} (expected {expected})")]
    FormatVersion { found: u32, expected: u32 },

    #[error("invalid argument: {0}")]
    InvalidArgument(String),
}

impl Error {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }
}
