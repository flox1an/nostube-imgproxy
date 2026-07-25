//! Size-bounded upstream body reads.
//!
//! `Response::bytes()` buffers whatever the upstream chooses to send before any
//! size check can run, so a hostile or misconfigured server could allocate
//! gigabytes regardless of `MAX_IMAGE_BYTES`. Everything that pulls a body from
//! an untrusted host goes through [`read_body_capped`] instead, which refuses
//! an oversized `Content-Length` up front and aborts mid-stream if the declared
//! length was a lie.

use bytes::{Bytes, BytesMut};
use reqwest::Response;

use crate::{error::SvcError, metrics};

/// Status used for "the source is bigger than we are willing to buffer".
///
/// Reported as an upstream status so Blossom candidate classification treats it
/// as a permanent fault for that server and moves on to the next candidate.
const TOO_LARGE: u16 = 413;

/// Read `response`'s body, failing fast once `max_bytes` would be exceeded.
pub async fn read_body_capped(mut response: Response, max_bytes: usize) -> Result<Bytes, SvcError> {
    if let Some(declared) = response.content_length() {
        if declared > max_bytes as u64 {
            metrics::record_processing_error("source_too_large");
            return Err(SvcError::UpstreamError(TOO_LARGE));
        }
    }

    // Trust the declared length only as an allocation hint, never as a bound.
    let hint = response
        .content_length()
        .map_or(0, |len| len.min(max_bytes as u64) as usize);
    let mut buffer = BytesMut::with_capacity(hint);

    while let Some(chunk) = response.chunk().await? {
        if buffer.len() + chunk.len() > max_bytes {
            metrics::record_processing_error("source_too_large");
            return Err(SvcError::UpstreamError(TOO_LARGE));
        }
        buffer.extend_from_slice(&chunk);
    }

    Ok(buffer.freeze())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{http::header, routing::get, Router};

    fn response_with(body: &'static [u8]) -> Response {
        Response::from(http::Response::new(body))
    }

    #[tokio::test]
    async fn read_body_capped_returns_a_body_within_the_limit() {
        let body = read_body_capped(response_with(b"tiny"), 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"tiny");
    }

    #[tokio::test]
    async fn read_body_capped_rejects_an_oversized_content_length() {
        crate::init_crypto_provider();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/large",
                    get(|| async { ([(header::CONTENT_LENGTH, "9999")], vec![0_u8; 9_999]) }),
                ),
            )
            .await
            .unwrap();
        });

        let response = reqwest::Client::new()
            .get(format!("http://{address}/large"))
            .send()
            .await
            .unwrap();
        let error = read_body_capped(response, 8)
            .await
            .expect_err("declared length over the cap must be refused");
        assert!(matches!(error, SvcError::UpstreamError(413)));
    }

    #[tokio::test]
    async fn read_body_capped_rejects_a_body_that_outgrows_an_absent_length() {
        // No Content-Length at all: the cap must still hold mid-stream.
        let error = read_body_capped(response_with(b"0123456789"), 4)
            .await
            .expect_err("streamed overflow must be refused");
        assert!(matches!(error, SvcError::UpstreamError(413)));
    }

    #[tokio::test]
    async fn read_body_capped_accepts_a_body_exactly_at_the_limit() {
        let body = read_body_capped(response_with(b"1234"), 4).await.unwrap();
        assert_eq!(body.len(), 4);
    }
}
