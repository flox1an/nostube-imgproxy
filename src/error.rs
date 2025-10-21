use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SvcError {
    #[error("bad request: {0}")]
    BadRequest(&'static str),
    #[error("fetch failed")]
    Fetch(#[from] reqwest::Error),
    #[error("decode failed")]
    Decode(#[from] image::ImageError),
    #[error("io failed")]
    Io(#[from] std::io::Error),
}

impl IntoResponse for SvcError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            SvcError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.to_string()),
            SvcError::Fetch(_) => (StatusCode::BAD_GATEWAY, "Failed to fetch source image".to_string()),
            SvcError::Decode(_) => (StatusCode::UNPROCESSABLE_ENTITY, "Failed to decode image".to_string()),
            SvcError::Io(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error".to_string()),
        };
        (status, message).into_response()
    }
}

