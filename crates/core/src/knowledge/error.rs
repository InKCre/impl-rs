use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

/// Knowledge failures distinguish unavailable contracts from absent projections.
#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Core(#[from] crate::Error),
    #[error("unknown exact Resolver: {0}")]
    UnknownResolver(String),
    #[error("unknown Storage handler: {0}")]
    UnknownStorage(String),
    #[error("unknown Resolver read method: {0}")]
    UnknownMethod(String),
    #[error("unsupported capability: {0}")]
    Unsupported(String),
    #[error("invalid content for {resolver}: {reason}")]
    InvalidContent {
        resolver: String,
        reason: &'static str,
    },
    #[error("invalid knowledge input: {0}")]
    InvalidInput(&'static str),
    #[error("capability registration conflicts: {0}")]
    Conflict(String),
    #[error("Storage operation failed: {0}")]
    Storage(&'static str),
    #[error("knowledge operation exceeded its explicit limit: {0}")]
    Limit(&'static str),
}

impl From<sqlx::Error> for Error {
    fn from(error: sqlx::Error) -> Self {
        Self::Core(error.into())
    }
}

impl From<Error> for crate::Error {
    fn from(error: Error) -> Self {
        match error {
            Error::Core(error) => error,
            Error::UnknownResolver(id) => {
                Self::Unsupported(format!("Resolver {id} is not registered"))
            }
            Error::UnknownStorage(id) => {
                Self::Unsupported(format!("Storage handler {id} is not registered"))
            }
            Error::Unsupported(capability) => Self::Unsupported(capability),
            Error::Conflict(reason) => Self::Conflict(reason),
            Error::Storage(reason) => Self::Unavailable(reason),
            error => Self::InvalidInput(error.to_string()),
        }
    }
}
