use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SvcError {
    #[error("bad request: {0}")]
    BadRequest(&'static str),
    #[error("upstream returned status {0}")]
    UpstreamError(u16),
    #[error("fetch failed")]
    Fetch(#[from] reqwest::Error),
    #[error("decode failed")]
    Decode(#[from] image::ImageError),
    #[error("io failed")]
    Io(#[from] std::io::Error),
    #[error("internal error: {0}")]
    InternalError(String),
    /// An already-rendered failure, used when one request's error is replayed
    /// to the other requests that were coalesced onto it.
    #[error("{1}")]
    Rendered(StatusCode, String),
}

impl SvcError {
    /// Collapse this error into the status and body the client will see.
    ///
    /// Split out of [`IntoResponse`] so a coalesced request can be handed the
    /// leader's rendered failure verbatim without cloning the original error.
    pub fn render(&self) -> (StatusCode, String) {
        match self {
            SvcError::BadRequest(msg) => (StatusCode::BAD_REQUEST, (*msg).to_string()),
            SvcError::UpstreamError(code) => {
                let status = StatusCode::from_u16(*code).unwrap_or(StatusCode::BAD_GATEWAY);
                let message = match code {
                    404 => "Source image not found".to_string(),
                    403 => "Source image forbidden".to_string(),
                    413 => "Source image too large".to_string(),
                    _ => format!("Upstream server returned status {}", code),
                };
                (status, message)
            }
            SvcError::Fetch(_) => (
                StatusCode::BAD_GATEWAY,
                "Failed to fetch source image".to_string(),
            ),
            SvcError::Decode(_) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "Failed to decode image".to_string(),
            ),
            SvcError::Io(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            ),
            SvcError::InternalError(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg.clone()),
            SvcError::Rendered(status, message) => (*status, message.clone()),
        }
    }
}

impl IntoResponse for SvcError {
    fn into_response(self) -> Response {
        self.render().into_response()
    }
}
