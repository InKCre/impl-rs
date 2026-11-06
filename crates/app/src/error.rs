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
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
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
        (
            self.status,
            Json(json!({"error":{"code": self.code,"message":self.message}})),
        )
            .into_response()
    }
}

impl From<inkcre_core::Error> for ApiError {
    fn from(error: inkcre_core::Error) -> Self {
        use inkcre_core::Error;
        let (status, code) = match &error {
            Error::InvalidInput(_) => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_input"),
            Error::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            Error::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            Error::Admission(_) | Error::Database(_) => {
                (StatusCode::SERVICE_UNAVAILABLE, "database_unavailable")
            }
            Error::Unsupported(_) => (StatusCode::NOT_IMPLEMENTED, "unsupported"),
        };
        Self::new(status, code, error.to_string())
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
