use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SvcError {
    #[error("bad request: {0}")]
    BadRequest(&'static str),
    #[error("forbidden: {0}")]
    Forbidden(&'static str),
    #[error("unauthorized")]
    Unauthorized,
    #[error("rate limited")]
    RateLimited,

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
    /// The node is at capacity and sheds this request rather than queueing it
    /// behind work it cannot finish inside the request budget.
    #[error("overloaded")]
    Overloaded,
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
            SvcError::Forbidden(msg) => (StatusCode::FORBIDDEN, (*msg).to_string()),
            SvcError::Unauthorized => (StatusCode::UNAUTHORIZED, "Unauthorized".to_string()),
            SvcError::RateLimited => (
                StatusCode::TOO_MANY_REQUESTS,
                "Rate limited, retry shortly".to_string(),
            ),
            // Upstream status codes are deliberately collapsed. Passing them
            // through made this service a scanning oracle for any host an
            // attacker could name via `xs=`, and let a third-party upstream pick
            // the status code a CDN in front of us caches.
            SvcError::UpstreamError(code) => match code {
                403 | 404 | 410 => (StatusCode::NOT_FOUND, "Source not found".to_string()),
                413 => (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Source too large".to_string(),
                ),
                _ => (
                    StatusCode::BAD_GATEWAY,
                    "Failed to fetch source".to_string(),
                ),
            },
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
            // The detail is logged in `into_response`, never returned: it can
            // carry a decoder panic payload with library and path internals.
            SvcError::InternalError(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            ),
            SvcError::Overloaded => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Overloaded, retry shortly".to_string(),
            ),
            SvcError::Rendered(status, message) => (*status, message.clone()),
        }
    }
}

impl IntoResponse for SvcError {
    fn into_response(self) -> Response {
        if let SvcError::InternalError(detail) = &self {
            tracing::error!(detail = %detail, "request failed internally");
        }

        let (status, body) = self.render();
        let mut response = (status, body).into_response();
        if status == StatusCode::SERVICE_UNAVAILABLE || status == StatusCode::TOO_MANY_REQUESTS {
            response.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from_static("1"),
            );
        }
        response
    }
}
