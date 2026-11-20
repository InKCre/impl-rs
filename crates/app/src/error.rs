use axum::{
    Json,
    extract::{FromRequest, FromRequestParts, Path, Query, Request},
    http::{StatusCode, request::Parts},
    response::{IntoResponse, Response},
};
use serde::de::DeserializeOwned;
use serde_json::json;

pub type ApiResult<T> = Result<T, ApiError>;

pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    constraint: Option<String>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            constraint: None,
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_input", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = json!({"error":{"code":self.code,"message":self.message}});
        if let Some(constraint) = self.constraint {
            body["error"]["constraint"] = json!(constraint);
        }
        (self.status, Json(body)).into_response()
    }
}

/// Only explicit canonical writes translate database-owned FK/CHECK conflicts.
/// Reads, transport failures and generated-key collisions retain server-failure semantics.
pub(crate) fn database_write(error: inkcre_core::Error) -> ApiError {
    if let inkcre_core::Error::DatabaseConstraint(sql_error)
    | inkcre_core::Error::Database(sql_error) = &error
        && let Some(database) = sql_error.as_database_error()
        && matches!(database.code().as_deref(), Some("23503" | "23514"))
    {
        let mut response = ApiError::new(
            StatusCode::CONFLICT,
            "constraint_conflict",
            database.message(),
        );
        response.constraint = database.constraint().map(str::to_owned);
        return response;
    }
    error.into()
}

/// Ordinary command admission treats an unknown type or unsupported command as invalid input.
/// Missing Source/Sink/Agent records retain their independent 404 semantics.
pub(crate) fn command_admission(error: inkcre_core::Error) -> ApiError {
    match error {
        inkcre_core::Error::NotFound("job type") => ApiError::invalid("unknown Job type"),
        inkcre_core::Error::Unsupported(_) => {
            ApiError::invalid("command type does not support this operation")
        }
        error => error.into(),
    }
}

impl From<inkcre_core::Error> for ApiError {
    fn from(error: inkcre_core::Error) -> Self {
        use inkcre_core::Error;
        if let Error::DatabaseConstraint(sql_error) = &error {
            let foreign_key = sql_error
                .as_database_error()
                .and_then(|database| database.code())
                .as_deref()
                == Some("23503");
            let message = if foreign_key {
                "invalid input: referenced record does not exist or is in use"
            } else {
                "invalid input: value violates the persisted contract"
            };
            return Self::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_input", message);
        }
        let (status, code) = match &error {
            Error::InvalidInput(_) | Error::DatabaseConstraint(_) => {
                (StatusCode::UNPROCESSABLE_ENTITY, "invalid_input")
            }
            Error::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            Error::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            Error::Admission(_) | Error::Database(_) => {
                (StatusCode::SERVICE_UNAVAILABLE, "database_unavailable")
            }
            Error::Unsupported(_) => (StatusCode::NOT_IMPLEMENTED, "unsupported"),
            Error::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "capability_unavailable"),
        };
        Self::new(status, code, error.to_string())
    }
}

impl From<inkcre_core::knowledge::Error> for ApiError {
    fn from(error: inkcre_core::knowledge::Error) -> Self {
        use inkcre_core::knowledge::Error;
        if let Error::Core(error) = error {
            return error.into();
        }
        let (status, code) = match &error {
            Error::UnknownResolver(_) | Error::UnknownStorage(_) | Error::Unsupported(_) => {
                (StatusCode::NOT_IMPLEMENTED, "unsupported")
            }
            Error::UnknownMethod(_) => (StatusCode::NOT_FOUND, "unknown_method"),
            Error::InvalidContent { .. } | Error::InvalidInput(_) => {
                (StatusCode::UNPROCESSABLE_ENTITY, "invalid_content")
            }
            Error::Conflict(_) => (StatusCode::CONFLICT, "capability_conflict"),
            Error::Storage(_) => (StatusCode::BAD_GATEWAY, "storage_unavailable"),
            Error::Limit(_) => (StatusCode::PAYLOAD_TOO_LARGE, "capability_limit"),
            Error::Core(_) => unreachable!(),
        };
        Self::new(status, code, error.to_string())
    }
}

pub struct ApiBytes(pub axum::body::Bytes);

impl<S: Send + Sync> FromRequest<S> for ApiBytes {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        axum::body::Bytes::from_request(request, state)
            .await
            .map(Self)
            .map_err(|error| {
                ApiError::new(
                    error.status(),
                    "invalid_body",
                    "unable to read request bytes",
                )
            })
    }
}

/// JSON extraction keeps parse failures inside the public error envelope.
pub struct ApiJson<T>(pub T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        Json::<T>::from_request(request, state)
            .await
            .map(|Json(value)| Self(value))
            .map_err(|error| {
                let message = if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    "request body exceeds the configured limit"
                } else {
                    "request body must contain valid JSON with the required fields"
                };
                ApiError::new(error.status(), "invalid_json", message)
            })
    }
}

pub struct ApiPath<T>(pub T);

impl<S, T> FromRequestParts<S> for ApiPath<T>
where
    S: Send + Sync,
    T: DeserializeOwned + Send,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Path::<T>::from_request_parts(parts, state)
            .await
            .map(|Path(value)| Self(value))
            .map_err(|_| ApiError::invalid("invalid path parameter"))
    }
}

pub struct ApiQuery<T>(pub T);

impl<S, T> FromRequestParts<S> for ApiQuery<T>
where
    S: Send + Sync,
    T: DeserializeOwned + Send,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(value)| Self(value))
            .map_err(|_| ApiError::invalid("invalid query parameter"))
    }
}
