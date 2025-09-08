use std::io;
use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("resctrl not mounted at {root}")]
    NotMounted { root: PathBuf },

    #[error("permission denied for {path}: {source}")]
    NoPermission { path: PathBuf, source: io::Error },

    #[error("resctrl capacity exhausted: {source}")]
    Capacity { source: io::Error },

    #[error("io error at {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
}

