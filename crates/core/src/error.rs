use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

/// Public errors keep driver diagnostics and input secrets out of HTTP/log text.
#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("{0} not found")]
    NotFound(&'static str),
    #[error("operation conflicts with current state: {0}")]
    Conflict(String),
    #[error("database protocol admission failed: {0}")]
    Admission(String),
    #[error("database operation failed")]
    Database(#[source] sqlx::Error),
    /// Preserve PostgreSQL reference/CHECK provenance for explicit write adapters.
    #[error("database reference or invariant conflict")]
    DatabaseConstraint(#[source] sqlx::Error),
    #[error("this runtime cannot execute the requested capability: {0}")]
    Unsupported(String),
    #[error("runtime capability is unavailable: {0}")]
    Unavailable(&'static str),
}

impl From<sqlx::Error> for Error {
    fn from(error: sqlx::Error) -> Self {
        if let sqlx::Error::Database(ref database) = error {
            match database.code().as_deref() {
                Some("23503" | "23514") => return Self::DatabaseConstraint(error),
                Some("23505") => return Self::Conflict("record already exists".into()),
                Some("23502" | "22P02") => {
                    return Self::InvalidInput("value violates the persisted contract".into());
                }
                _ => {}
            }
        }
        Self::Database(error)
    }
}
