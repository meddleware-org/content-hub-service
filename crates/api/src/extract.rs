//! Request extractors that answer in the API's error format (axum's own rejections are plain text).

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};

use crate::error::ApiError;

/// A JSON body; malformed JSON or an unknown field is a 422 with the reason.
pub struct Body<T>(pub T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for Body<T> {
    /// A bad body is answered as an API error.
    type Rejection = ApiError;
    /// Read the JSON body, naming the reason for a refusal.
    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(v)) => Ok(Body(v)),
            // A body without the JSON content type is a 400.
            Err(JsonRejection::MissingJsonContentType(_)) => {
                Err(ApiError::BadRequest("Content-Type must be application/json".into()))
            }
            Err(JsonRejection::BytesRejection(_)) => {
                Err(ApiError::BadRequest("the body could not be read (too large?)".into()))
            }
            // Anything else (bad JSON, an unknown field, a wrong type) is a 422 that says why.
            Err(e) => Err(ApiError::Invalid(e.body_text())),
        }
    }
}

/// Query parameters; unknown parameters are refused (they would otherwise be silently ignored).
pub struct Query<T>(pub T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequestParts<S> for Query<T> {
    /// A bad query is answered as an API error.
    type Rejection = ApiError;
    /// Read the query, refusing unknown parameters.
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(v)) => Ok(Query(v)),
            // An unknown or malformed parameter is refused, not ignored.
            Err(QueryRejection::FailedToDeserializeQueryString(e)) => Err(ApiError::BadRequest(e.body_text())),
            Err(e) => Err(ApiError::BadRequest(e.body_text())),
        }
    }
}

/// For PATCH bodies: distinguishes an absent field (`None`) from an explicit `null` (`Some(None)`).
pub fn nullable<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}
