//! The API's error model: a status, a stable machine-readable code and a human message. Database errors are mapped
//! by SQLSTATE; nothing internal (SQL, connection strings, stack traces) reaches a response.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use content_hub_store::StoreError;
use serde::Serialize;

/// The JSON body of every error answer.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ErrorBody {
    /// Stable code, e.g. `not_found`, `conflict`, `invalid`, `unauthorized`, `forbidden`.
    pub error: &'static str,
    /// A sentence for the person; it never repeats a credential or a database message.
    pub message: String,
}

/// Why a request failed, and the status each reason answers with.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// 400: the request could not be read.
    #[error("{0}")]
    BadRequest(String),
    /// 401: no valid credential.
    #[error("authentication required")]
    Unauthorized,
    /// 403: not allowed; the reason follows.
    #[error("{0}")]
    Forbidden(&'static str),
    /// 404: there is no such thing.
    #[error("not found")]
    NotFound,
    /// 409: the change conflicts with what is stored.
    #[error("{0}")]
    Conflict(String),
    /// 422: the values are well formed but not acceptable.
    #[error("{0}")]
    Invalid(String),
    /// 503: not ready or temporarily unable; the reason follows.
    #[error("{0}")]
    Unavailable(&'static str),
    /// 500: something went wrong that the caller cannot fix; the detail goes to the log only.
    #[error("internal error")]
    Internal,
}

impl ApiError {
    /// The status and the stable code the error answers with.
    fn parts(&self) -> (StatusCode, &'static str) {
        match self {
            ApiError::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            ApiError::Forbidden(_) => (StatusCode::FORBIDDEN, "forbidden"),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            ApiError::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            ApiError::Invalid(_) => (StatusCode::UNPROCESSABLE_ENTITY, "invalid"),
            ApiError::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
            ApiError::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        }
    }
}

impl IntoResponse for ApiError {
    /// Answer with the error's status and a JSON body.
    fn into_response(self) -> Response {
        let (status, error) = self.parts();
        (status, Json(ErrorBody { error, message: self.to_string() })).into_response()
    }
}

/// Map a database error to an API error by its constraint, without repeating the database's message.
fn from_db(e: &sqlx::Error) -> ApiError {
    if let Some(d) = e.as_database_error() {
        let constraint = d.constraint().unwrap_or("a constraint");
        return match d.code().as_deref() {
            // Unique violation.
            Some("23505") => ApiError::Conflict(format!("already exists ({constraint})")),
            // Foreign key or restrict violation: something still depends on it, or it points at nothing.
            Some("23503") | Some("23001") => {
                ApiError::Conflict(format!("still referenced or refers to something missing ({constraint})"))
            }
            // Check, not-null, bad text format, too long, bad date and out-of-range date: the data is not acceptable.
            Some("23514") | Some("23502") | Some("22P02") | Some("22001") | Some("22007") | Some("22008") => {
                ApiError::Invalid(format!("refused by the database ({constraint})"))
            }
            // Serialisation failure and deadlock: the caller may retry.
            Some("40001") | Some("40P01") => ApiError::Unavailable("concurrent change; retry"),
            // A statement timeout.
            Some("57014") => ApiError::Unavailable("the database took too long"),
            // Permission denied means a role is wrong, which is an operator problem and is logged as one.
            Some("42501") => {
                tracing::error!(code = "42501", "permission denied in the database: a role is misconfigured");
                ApiError::Internal
            }
            _ => {
                tracing::error!(code = ?d.code(), "database error");
                ApiError::Internal
            }
        };
    }
    match e {
        // The database cannot be reached.
        sqlx::Error::PoolTimedOut | sqlx::Error::Io(_) | sqlx::Error::Tls(_) | sqlx::Error::PoolClosed => {
            ApiError::Unavailable("database unavailable")
        }
        sqlx::Error::RowNotFound => ApiError::NotFound,
        other => {
            tracing::error!(error = %other, "database error");
            ApiError::Internal
        }
    }
}

impl From<sqlx::Error> for ApiError {
    /// A database error, mapped without repeating its message.
    fn from(e: sqlx::Error) -> Self {
        from_db(&e)
    }
}

impl From<StoreError> for ApiError {
    /// A store error: a database error is mapped, anything else is logged and answers 500.
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Db(e) => from_db(&e),
            other => {
                tracing::error!(error = %other, "store error");
                ApiError::Internal
            }
        }
    }
}

/// An API result.
pub type ApiResult<T> = Result<T, ApiError>;
