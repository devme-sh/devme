use std::path::PathBuf;

use thiserror::Error;

/// All ways slot allocation can fail.
#[derive(Debug, Error)]
pub enum AllocError {
    #[error("no free slot available (max = {max})")]
    Exhausted { max: u8 },

    #[error("registry file at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("registry file at {path} is corrupt: {source}")]
    Corrupt {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("could not serialize registry: {0}")]
    Encode(#[from] toml::ser::Error),

    #[error("failed to acquire registry lock at {path}: {source}")]
    Lock {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}
