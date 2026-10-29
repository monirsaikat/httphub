use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("parse error in {file} at line {line}: {message}")]
    Parse {
        file: String,
        line: usize,
        message: String,
    },

    #[error("unresolved variable `{0}` (define it in an environment or pass --var {0}=value)")]
    UnresolvedVar(String),

    #[error("no request named `{0}` in that file")]
    NoSuchRequest(String),

    #[error("file is empty of requests: {0}")]
    NoRequests(PathBuf),

    #[error("invalid url `{url}`: {message}")]
    InvalidUrl { url: String, message: String },

    #[error("dns lookup for `{host}` returned no addresses")]
    DnsEmpty { host: String },

    #[error("request timed out after {0:?}")]
    Timeout(std::time::Duration),

    #[error("transport: {0}")]
    Transport(String),

    #[error("tls: {0}")]
    Tls(String),

    #[error("{0}")]
    Config(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<hyper::Error> for Error {
    fn from(e: hyper::Error) -> Self {
        Error::Transport(e.to_string())
    }
}

impl From<http::Error> for Error {
    fn from(e: http::Error) -> Self {
        Error::Transport(e.to_string())
    }
}
