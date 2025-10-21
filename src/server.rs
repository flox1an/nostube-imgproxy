use axum::{
    body::Body,
    extract::{Path as AxPath, State},
    http::{header, HeaderValue, StatusCode},
    response::Response,
    routing::get,
    Router,
};
use bytes::Bytes;
use http::HeaderName;

use crate::{
    cache::{cache_path_for, original_cache_path_for, try_read_original_cache, try_serve_cache, write_cache_atomic},
    config::AppState,
    error::SvcError,
    transform::{apply_resize, encode_image, parse_rest},
};

/// Create the Axum router with all routes
pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/insecure/*rest", get(handle_insecure))
        .with_state(state)
}

/// Main handler for /insecure/* requests
async fn handle_insecure(
    State(state): State<AppState>,
    AxPath(rest): AxPath<String>,
) -> Result<Response, SvcError> {
    // full_url is the exact request path for cache keying
    let full_request_url = format!("/insecure/{}", rest);

    // Parse something like: f:webp/q:85/rs:fill:480:480/plain/<encoded>
    let (dirs, src_url) = parse_rest(&rest)?;

    // Derive cache file path from hash(full_request_url)
    let cache_path = cache_path_for(&state.cfg, &full_request_url, &dirs.out_fmt);
    let mime = dirs.out_fmt.mime_type();

    // Serve from processed cache if present
    if let Some(resp) = try_serve_cache(&cache_path, mime).await? {
        return Ok(resp);
    }

    // Try to get original image from cache first, otherwise fetch it
    let original_cache_path = original_cache_path_for(&state.cfg, &src_url);
    let img_bytes = if let Some(cached) = try_read_original_cache(&original_cache_path).await? {
        cached
    } else {
        // Fetch from source
        let bytes = fetch_source(&state, &src_url).await?;
        
        // Ensure max size
        if bytes.len() > state.cfg.max_image_bytes {
            return Err(SvcError::BadRequest("image too large"));
        }
        
        // Cache the original
        write_cache_atomic(&original_cache_path, &bytes).await?;
        bytes.to_vec()
    };

    // Decode - use ImageReader with format guessing for AVIF/JPEG/PNG/WebP support
    let img = {
        use std::io::Cursor;
        image::ImageReader::new(Cursor::new(&img_bytes))
            .with_guessed_format()
            .map_err(|e| SvcError::Decode(image::ImageError::IoError(e)))?
            .decode()?
    };

    // Transform
    let img = apply_resize(img, &dirs.resize);

    // Encode
    let encoded = encode_image(&img, &dirs.out_fmt, dirs.quality)?;

    // Write to cache atomically
    write_cache_atomic(&cache_path, &encoded).await?;

    let mut resp = Response::new(Body::from(encoded));
    *resp.status_mut() = StatusCode::OK;
    let headers = resp.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(mime).unwrap());
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=3600, stale-while-revalidate=600"),
    );
    headers.insert(
        HeaderName::from_static("x-cache"),
        HeaderValue::from_static("miss"),
    );

    Ok(resp)
}

/// Fetch source image from URL
async fn fetch_source(state: &AppState, src_url: &str) -> Result<Bytes, SvcError> {
    // Basic allowlist: only http/https
    if !(src_url.starts_with("http://") || src_url.starts_with("https://")) {
        return Err(SvcError::BadRequest("unsupported source scheme"));
    }

    let resp = state.http.get(src_url).send().await?;
    if !resp.status().is_success() {
        return Err(SvcError::BadRequest("upstream not ok"));
    }

    let bytes = resp.bytes().await?;
    Ok(bytes)
}

