//! The API's error model: a status, a stable machine-readable code and a human message. Database errors are mapped
//! by SQLSTATE; nothing internal (SQL, connection strings, stack traces) reaches a response.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use content_hub_store::StoreError;
use serde::Serialize;

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ErrorBody {
    /// Stable code, e.g. `not_found`, `conflict`, `invalid`, `unauthorized`, `forbidden`.
    pub error: &'static str,
    pub message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("authentication required")]
    Unauthorized,
    #[error("{0}")]
    Forbidden(&'static str),
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Unavailable(&'static str),
    #[error("internal error")]
    Internal,
}

impl ApiError {
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
    fn into_response(self) -> Response {
        let (status, error) = self.parts();
        (status, Json(ErrorBody { error, message: self.to_string() })).into_response()
    }
}

fn from_db(e: &sqlx::Error) -> ApiError {
    if let Some(d) = e.as_database_error() {
        let constraint = d.constraint().unwrap_or("a constraint");
        return match d.code().as_deref() {
            Some("23505") => ApiError::Conflict(format!("already exists ({constraint})")),
            Some("23503") | Some("23001") => {
                ApiError::Conflict(format!("still referenced or refers to something missing ({constraint})"))
            }
            Some("23514") | Some("23502") | Some("22P02") | Some("22001") | Some("22007") | Some("22008") => {
                ApiError::Invalid(format!("refused by the database ({constraint})"))
            }
            Some("40001") | Some("40P01") => ApiError::Unavailable("concurrent change; retry"),
            Some("57014") => ApiError::Unavailable("the database took too long"),
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
    fn from(e: sqlx::Error) -> Self {
        from_db(&e)
    }
}

impl From<StoreError> for ApiError {
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

pub type ApiResult<T> = Result<T, ApiError>;
