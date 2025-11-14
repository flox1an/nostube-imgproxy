use axum::{
    body::Body,
    extract::{Path as AxPath, Query, State},
    http::{header, HeaderValue, StatusCode},
    response::Response,
    routing::get,
    Router,
};
use bytes::Bytes;
use http::HeaderName;
use serde::Deserialize;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};

use crate::{
    blossom::{combine_server_lists, BlossomState},
    cache::{cache_path_for, original_cache_path_for, try_read_original_cache, try_serve_cache, write_cache_atomic},
    config::AppState,
    error::SvcError,
    thumbnail::{extract_video_thumbnail, is_video_url, ThumbnailState},
    transform::{apply_resize, encode_image, parse_rest, Directives, OutFmt, Resize, ResizeMode},
};

/// Combined state for image and video processing
#[derive(Clone)]
pub struct CombinedState {
    pub app: AppState,
    pub thumbnail: Arc<ThumbnailState>,
    pub blossom: Arc<BlossomState>,
}

/// Create the Axum router with all routes
pub fn create_router(
    state: AppState,
    thumbnail_state: Arc<ThumbnailState>,
    blossom_state: Arc<BlossomState>,
) -> Router {
    let combined = CombinedState {
        app: state,
        thumbnail: thumbnail_state,
        blossom: blossom_state,
    };

    // CORS layer - allow all origins
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .route("/insecure/{*rest}", get(handle_insecure))
        .route("/thumb/{filename}", get(handle_thumb))
        .route("/health", get(health_check))
        .with_state(combined)
        .layer(cors)
}

/// Query parameters for /thumb endpoint
#[derive(Debug, Deserialize)]
struct ThumbQuery {
    /// Output format (e.g., "webp", "jpeg", "png", "avif")
    #[serde(rename = "f")]
    format: Option<String>,

    /// Resize directive (e.g., "fit:480:480", "fill:400:400")
    #[serde(rename = "rs")]
    resize: Option<String>,

    /// Quality (0-100)
    #[serde(rename = "q")]
    quality: Option<u8>,

    /// Server hints (can be multiple)
    #[serde(rename = "xs", default)]
    server_hints: Vec<String>,

    /// Author pubkey for Nostr-based lookup
    #[serde(rename = "as")]
    author_pubkey: Option<String>,
}

/// Simple health check endpoint
async fn health_check() -> &'static str {
    "OK"
}

/// Main handler for /insecure/{*} requests (handles both images and videos)
async fn handle_insecure(
    State(state): State<CombinedState>,
    AxPath(rest): AxPath<String>,
) -> Result<Response, SvcError> {
    // full_url is the exact request path for cache keying
    let full_request_url = format!("/insecure/{}", rest);

    // Parse something like: f:webp/q:85/rs:fill:480:480/plain/<encoded>
    let (dirs, src_url) = parse_rest(&rest)?;

    // Derive cache file path from hash(full_request_url)
    let cache_path = cache_path_for(&state.app.cfg, &full_request_url, &dirs.out_fmt);
    let mime = dirs.out_fmt.mime_type();

    // Serve from processed cache if present
    if let Some(resp) = try_serve_cache(&cache_path, mime).await? {
        return Ok(resp);
    }

    // Try to get original image/video thumbnail from cache first
    let original_cache_path = original_cache_path_for(&state.app.cfg, &src_url);
    let img_bytes = if let Some(cached) = try_read_original_cache(&original_cache_path).await? {
        // Cache hit - use cached original (could be image or previously extracted thumbnail)
        cached
    } else {
        // Cache miss - check if source is a video or image
        if is_video_url(&src_url) {
            // It's a video - extract thumbnail using FFmpeg
            let thumbnail_bytes = extract_video_thumbnail(
                &src_url,
                &state.thumbnail.ffmpeg_semaphore,
                &state.app.cfg.blossom_fallback_servers,
            ).await?;
            
            // Ensure max size
            if thumbnail_bytes.len() > state.app.cfg.max_image_bytes {
                return Err(SvcError::BadRequest("thumbnail too large"));
            }
            
            // Cache the extracted thumbnail as "original"
            write_cache_atomic(&original_cache_path, &thumbnail_bytes).await?;
            thumbnail_bytes
        } else {
            // It's an image - fetch normally
            let bytes = fetch_source(&state.app, &src_url).await?;
            
            // Ensure max size
            if bytes.len() > state.app.cfg.max_image_bytes {
                return Err(SvcError::BadRequest("image too large"));
            }
            
            // Cache the original image
            write_cache_atomic(&original_cache_path, &bytes).await?;
            bytes.to_vec()
        }
    };

    // Decode - use ImageReader with content-based format detection
    // Supports: JPEG, JFIF, PNG, WebP, AVIF, and other formats
    // Works with or without file extensions (detects format from image data)
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
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    headers.insert(
        HeaderName::from_static("x-cache"),
        HeaderValue::from_static("miss"),
    );

    Ok(resp)
}

/// Handler for /thumb/<sha256>.<ext> endpoint (Blossom-specialized)
async fn handle_thumb(
    State(state): State<CombinedState>,
    AxPath(filename): AxPath<String>,
    Query(params): Query<ThumbQuery>,
) -> Result<Response, SvcError> {
    // Validate filename format: <sha256>.<ext>
    let (hash, ext) = filename
        .rsplit_once('.')
        .ok_or(SvcError::BadRequest("invalid filename format, expected <sha256>.<ext>"))?;

    // Validate SHA256 hash (64 hex characters)
    if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(SvcError::BadRequest("invalid SHA256 hash"));
    }

    // Parse directives from query parameters
    let dirs = parse_thumb_params(&params)?;

    // Build cache key from full request (path + query params)
    let cache_key = format!("/thumb/{}?{}", filename, build_query_string(&params));
    let cache_path = cache_path_for(&state.app.cfg, &cache_key, &dirs.out_fmt);
    let mime = dirs.out_fmt.mime_type();

    // Serve from processed cache if present
    if let Some(resp) = try_serve_cache(&cache_path, mime).await? {
        return Ok(resp);
    }

    // Get author servers if pubkey provided
    let author_servers = if let Some(ref pubkey) = params.author_pubkey {
        match state.blossom.get_author_servers(pubkey).await {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("Failed to fetch author servers for pubkey {}: {}", pubkey, e);
                None
            }
        }
    } else {
        None
    };

    // Combine servers: xs (highest priority) -> as -> fallback
    let servers = combine_server_lists(
        if params.server_hints.is_empty() {
            None
        } else {
            Some(&params.server_hints)
        },
        author_servers.as_deref(),
        &state.app.cfg.blossom_fallback_servers,
    );

    tracing::debug!("Resolved {} servers for {}.{}: {:?}", servers.len(), hash, ext, servers);

    // Try to fetch from servers in order
    let original_cache_key = format!("{}.{}", hash, ext);
    let original_cache_path = original_cache_path_for(&state.app.cfg, &original_cache_key);

    // Check original cache first
    let img_bytes = if let Some(cached) = try_read_original_cache(&original_cache_path).await? {
        tracing::debug!("Original cache hit for {}.{}", hash, ext);
        cached
    } else {
        // Fetch from Blossom servers
        let bytes = fetch_from_blossom_servers(&state.app, &servers, hash, ext).await?;

        // Validate size
        if bytes.len() > state.app.cfg.max_image_bytes {
            return Err(SvcError::BadRequest("image too large"));
        }

        // Cache the original
        write_cache_atomic(&original_cache_path, &bytes).await?;
        bytes.to_vec()
    };

    // Decode image
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

    // Write to processed cache
    write_cache_atomic(&cache_path, &encoded).await?;

    // Build response
    let mut resp = Response::new(Body::from(encoded));
    *resp.status_mut() = StatusCode::OK;
    let headers = resp.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(mime).unwrap());
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    headers.insert(
        HeaderName::from_static("x-cache"),
        HeaderValue::from_static("miss"),
    );

    Ok(resp)
}

/// Parse thumb query parameters into Directives
fn parse_thumb_params(params: &ThumbQuery) -> Result<Directives, SvcError> {
    // Parse output format
    let out_fmt = if let Some(ref fmt) = params.format {
        match fmt.to_ascii_lowercase().as_str() {
            "jpeg" | "jpg" => OutFmt::Jpeg,
            "png" => OutFmt::Png,
            "webp" => OutFmt::Webp,
            "avif" => OutFmt::Avif,
            _ => return Err(SvcError::BadRequest("unsupported format")),
        }
    } else {
        OutFmt::Webp // Default to WebP for Blossom thumbs
    };

    // Parse quality
    let quality = params.quality.unwrap_or(82);
    if quality > 100 {
        return Err(SvcError::BadRequest("quality must be 0-100"));
    }

    // Parse resize directive
    let resize = if let Some(ref rs) = params.resize {
        parse_resize_from_query(rs)?
    } else {
        // Default: fit 480x480
        Resize {
            mode: ResizeMode::Fit,
            w: 480,
            h: 480,
        }
    };

    Ok(Directives {
        out_fmt,
        quality,
        resize,
    })
}

/// Parse resize directive from query param (e.g., "fit:480:480")
fn parse_resize_from_query(rs: &str) -> Result<Resize, SvcError> {
    let parts: Vec<&str> = rs.split(':').collect();
    if parts.len() != 3 {
        return Err(SvcError::BadRequest("invalid resize format, expected mode:width:height"));
    }

    let mode = match parts[0].to_ascii_lowercase().as_str() {
        "fit" => ResizeMode::Fit,
        "fill" => ResizeMode::Fill,
        "fill-down" => ResizeMode::FillDown,
        "force" => ResizeMode::Force,
        "auto" => ResizeMode::Auto,
        _ => return Err(SvcError::BadRequest("unsupported resize mode")),
    };

    let w = parts[1].parse().unwrap_or(0);
    let h = parts[2].parse().unwrap_or(0);

    Ok(Resize { mode, w, h })
}

/// Build query string for cache key
fn build_query_string(params: &ThumbQuery) -> String {
    let mut parts = Vec::new();

    if let Some(ref f) = params.format {
        parts.push(format!("f={}", f));
    }
    if let Some(ref rs) = params.resize {
        parts.push(format!("rs={}", rs));
    }
    if let Some(q) = params.quality {
        parts.push(format!("q={}", q));
    }
    for xs in &params.server_hints {
        parts.push(format!("xs={}", xs));
    }
    if let Some(ref as_) = params.author_pubkey {
        parts.push(format!("as={}", as_));
    }

    parts.join("&")
}

/// Fetch image from Blossom servers (try each in order)
async fn fetch_from_blossom_servers(
    state: &AppState,
    servers: &[String],
    hash: &str,
    ext: &str,
) -> Result<Bytes, SvcError> {
    if servers.is_empty() {
        return Err(SvcError::BadRequest("no servers available to fetch from"));
    }

    let mut last_error = None;

    for (idx, server) in servers.iter().enumerate() {
        let url = format!("{}/{}.{}", server.trim_end_matches('/'), hash, ext);
        tracing::debug!("Attempting server {}/{}: {}", idx + 1, servers.len(), url);

        match state.http.get(&url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    match resp.bytes().await {
                        Ok(bytes) => {
                            tracing::info!(
                                "✓ Server {}/{} succeeded: {} ({} bytes)",
                                idx + 1,
                                servers.len(),
                                server,
                                bytes.len()
                            );
                            return Ok(bytes);
                        }
                        Err(e) => {
                            tracing::debug!("✗ Server {}/{} failed to read bytes: {:?}", idx + 1, servers.len(), e);
                            last_error = Some(SvcError::UpstreamError(500));
                        }
                    }
                } else {
                    tracing::debug!(
                        "✗ Server {}/{} returned status {}: {}",
                        idx + 1,
                        servers.len(),
                        status,
                        server
                    );
                    last_error = Some(SvcError::UpstreamError(status.as_u16()));
                }
            }
            Err(e) => {
                tracing::debug!("✗ Server {}/{} request failed: {:?}", idx + 1, servers.len(), e);
                last_error = Some(SvcError::UpstreamError(500));
            }
        }
    }

    tracing::warn!("All {} servers failed for {}.{}", servers.len(), hash, ext);

    // Return the last error or a generic not found
    Err(last_error.unwrap_or(SvcError::UpstreamError(404)))
}

/// Check if a URL is a Blossom CDN URL (has <sha256>.<ext> format)
fn is_blossom_url(url: &str) -> bool {
    if let Some(filename) = url.rsplit('/').next() {
        if let Some((hash_part, _ext)) = filename.rsplit_once('.') {
            // SHA256 hash is 64 hexadecimal characters
            return hash_part.len() == 64 && hash_part.chars().all(|c| c.is_ascii_hexdigit());
        }
    }
    false
}

/// Extract the hash and extension from a Blossom URL
/// Returns (hash, extension) if valid, None otherwise
fn extract_blossom_hash(url: &str) -> Option<(&str, &str)> {
    if let Some(filename) = url.rsplit('/').next() {
        if let Some((hash_part, ext)) = filename.rsplit_once('.') {
            // SHA256 hash is 64 hexadecimal characters
            if hash_part.len() == 64 && hash_part.chars().all(|c| c.is_ascii_hexdigit()) {
                return Some((hash_part, ext));
            }
        }
    }
    None
}

/// Fetch source image from URL with Blossom fallback support
async fn fetch_source(state: &AppState, src_url: &str) -> Result<Bytes, SvcError> {
    // Basic allowlist: only http/https
    if !(src_url.starts_with("http://") || src_url.starts_with("https://")) {
        return Err(SvcError::BadRequest("unsupported source scheme"));
    }

    // Try original URL first
    let result = async {
        let resp = state.http.get(src_url).send().await?;
        let status = resp.status();
        if status.is_success() {
            resp.bytes().await.map_err(Into::into)
        } else {
            tracing::debug!("primary server returned non-success status for image {}: {}", src_url, status);
            Err(SvcError::UpstreamError(status.as_u16()))
        }
    }.await;

    // If successful, return immediately
    if let Ok(bytes) = &result {
        tracing::debug!("primary server succeeded for image {}, received {} bytes", src_url, bytes.len());
        return Ok(bytes.clone());
    }

    // Log primary failure
    tracing::debug!("primary server failed for image {}: {:?}", src_url, result);

    // If failed and it's a Blossom URL, try fallback servers
    if is_blossom_url(src_url) {
        tracing::debug!("url is blossom format, attempting {} fallback servers", state.cfg.blossom_fallback_servers.len());

        if let Some((hash, ext)) = extract_blossom_hash(src_url) {
            // Try each fallback server
            for (idx, fallback_server) in state.cfg.blossom_fallback_servers.iter().enumerate() {
                let fallback_url = format!("{}/{}.{}", fallback_server.trim_end_matches('/'), hash, ext);
                tracing::debug!(
                    "attempting fallback server {}/{} for image: {}",
                    idx + 1,
                    state.cfg.blossom_fallback_servers.len(),
                    fallback_url
                );

                match state.http.get(&fallback_url).send().await {
                    Ok(fallback_resp) => {
                        let status = fallback_resp.status();
                        if status.is_success() {
                            match fallback_resp.bytes().await {
                                Ok(bytes) => {
                                    tracing::info!(
                                        "✓ fallback server {} succeeded for image, received {} bytes from {}",
                                        idx + 1,
                                        bytes.len(),
                                        fallback_server
                                    );
                                    return Ok(bytes);
                                }
                                Err(e) => {
                                    tracing::debug!(
                                        "✗ fallback server {} failed to read response bytes: {:?}",
                                        idx + 1,
                                        e
                                    );
                                }
                            }
                        } else {
                            tracing::debug!(
                                "✗ fallback server {} returned status {} for {}",
                                idx + 1,
                                status,
                                fallback_server
                            );
                        }
                    }
                    Err(e) => {
                        tracing::debug!(
                            "✗ fallback server {} request failed for {}: {:?}",
                            idx + 1,
                            fallback_server,
                            e
                        );
                    }
                }
            }

            tracing::warn!(
                "all {} fallback servers exhausted for image {}, returning original error",
                state.cfg.blossom_fallback_servers.len(),
                src_url
            );
        }
    } else {
        tracing::debug!("url is not blossom format, skipping fallback servers");
    }

    // All attempts failed - return original error
    result
}

