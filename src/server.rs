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
    blossom::{combine_server_lists, extract_blossom_hash, fetch_from_servers, is_blossom_url, BlossomState},
    cache::{cache_path_for, original_cache_path_for, try_read_original_cache, try_serve_cache, write_cache_atomic},
    config::AppState,
    error::SvcError,
    metrics,
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
        .route("/metrics", get(handle_metrics))
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

    /// Server hints — hostnames or full URLs (xs= can repeat)
    #[serde(rename = "xs", default)]
    server_hints: Vec<String>,

    /// Author pubkey (npub or hex) for kind 10063 relay lookup
    #[serde(rename = "as")]
    author_pubkey: Option<String>,

    /// Max output width in pixels (from nostube proxyConfig.maxSize)
    width: Option<u32>,

    /// Max output height in pixels (from nostube proxyConfig.maxSize)
    height: Option<u32>,
}

/// Simple health check endpoint
async fn health_check() -> &'static str {
    "OK"
}

/// Prometheus metrics endpoint
async fn handle_metrics() -> Result<Response, SvcError> {
    let metrics_text = metrics::encode_metrics()
        .map_err(|e| SvcError::InternalError(format!("failed to encode metrics: {}", e)))?;

    let mut resp = Response::new(Body::from(metrics_text));
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
    );

    Ok(resp)
}

/// Main handler for /insecure/{*} requests (handles both images and videos)
async fn handle_insecure(
    State(state): State<CombinedState>,
    AxPath(rest): AxPath<String>,
) -> Result<Response, SvcError> {
    let start_time = std::time::Instant::now();

    // full_url is the exact request path for cache keying
    let full_request_url = format!("/insecure/{}", rest);

    // Parse something like: f:webp/q:85/rs:fill:480:480/plain/<encoded>
    let (dirs, src_url) = parse_rest(&rest)?;

    // Derive cache file path from hash(full_request_url)
    let cache_path = cache_path_for(&state.app.cfg, &full_request_url, &dirs.out_fmt);
    let mime = dirs.out_fmt.mime_type();

    // Serve from processed cache if present
    if let Some(resp) = try_serve_cache(&cache_path, mime).await? {
        metrics::record_cache_hit("processed");
        let duration = start_time.elapsed().as_secs_f64();
        metrics::observe_http_duration("/insecure", "GET", duration);
        metrics::record_http_request("/insecure", "GET", 200);
        return Ok(resp);
    }

    metrics::record_cache_miss("processed");

    // Try to get original image/video thumbnail from cache first
    let original_cache_path = original_cache_path_for(&state.app.cfg, &src_url);
    let img_bytes = if let Some(cached) = try_read_original_cache(&original_cache_path).await? {
        metrics::record_cache_hit("original");
        // Cache hit - use cached original (could be image or previously extracted thumbnail)
        cached
    } else {
        metrics::record_cache_miss("original");
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
                metrics::record_processing_error("thumbnail_too_large");
                return Err(SvcError::BadRequest("thumbnail too large"));
            }

            metrics::record_bytes_downloaded("video", thumbnail_bytes.len());

            // Cache the extracted thumbnail as "original"
            write_cache_atomic(&original_cache_path, &thumbnail_bytes).await?;
            thumbnail_bytes
        } else {
            // It's an image - fetch normally
            let bytes = fetch_source(&state.app, &src_url).await?;

            // Ensure max size
            if bytes.len() > state.app.cfg.max_image_bytes {
                metrics::record_processing_error("image_too_large");
                return Err(SvcError::BadRequest("image too large"));
            }

            metrics::record_bytes_downloaded("image", bytes.len());

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

    // Record processing metrics
    let out_fmt_str = match dirs.out_fmt {
        OutFmt::Jpeg => "jpeg",
        OutFmt::Png => "png",
        OutFmt::Webp => "webp",
        OutFmt::Avif => "avif",
    };

    if is_video_url(&src_url) {
        metrics::record_video_processed(out_fmt_str);
    } else {
        metrics::record_image_processed(out_fmt_str);
    }

    metrics::record_bytes_served(mime, encoded.len());

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

    // Record request metrics
    let duration = start_time.elapsed().as_secs_f64();
    metrics::observe_http_duration("/insecure", "GET", duration);
    metrics::record_http_request("/insecure", "GET", 200);

    Ok(resp)
}

/// Handler for /thumb/<sha256>.<ext> endpoint (Blossom-specialized)
async fn handle_thumb(
    State(state): State<CombinedState>,
    AxPath(filename): AxPath<String>,
    Query(params): Query<ThumbQuery>,
) -> Result<Response, SvcError> {
    let start_time = std::time::Instant::now();

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
        metrics::record_cache_hit("processed");
        let duration = start_time.elapsed().as_secs_f64();
        metrics::observe_http_duration("/thumb", "GET", duration);
        metrics::record_http_request("/thumb", "GET", 200);
        return Ok(resp);
    }

    metrics::record_cache_miss("processed");

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

    // Build a representative video URL for cache keying and ffmpeg
    // (server resolution happens separately below)
    let is_video = is_video_url(&format!("{}.{}", hash, ext));
    let original_cache_key = format!("{}.{}", hash, ext);
    let original_cache_path = original_cache_path_for(&state.app.cfg, &original_cache_key);

    // Check original cache first (stores thumbnail bytes for video hashes)
    let img_bytes = if let Some(cached) = try_read_original_cache(&original_cache_path).await? {
        metrics::record_cache_hit("original");
        tracing::debug!("original cache hit for {}.{}", hash, ext);
        cached
    } else {
        metrics::record_cache_miss("original");

        if is_video {
            // Video hash: pick the first server URL and pass it to ffmpeg.
            // ffmpeg streams the video — we don't buffer it in memory.
            if servers.is_empty() {
                return Err(SvcError::BadRequest("no servers available for video thumbnail"));
            }
            // Build the primary URL from the top-priority server; the remaining
            // servers are passed as the fallback list so extract_video_thumbnail
            // can try them in order.
            let primary_url = format!("{}/{}.{}", servers[0].trim_end_matches('/'), hash, ext);
            let thumbnail_bytes = extract_video_thumbnail(
                &primary_url,
                &state.thumbnail.ffmpeg_semaphore,
                &servers,
            )
            .await?;

            if thumbnail_bytes.len() > state.app.cfg.max_image_bytes {
                metrics::record_processing_error("thumbnail_too_large");
                return Err(SvcError::BadRequest("thumbnail too large"));
            }

            metrics::record_bytes_downloaded("video", thumbnail_bytes.len());
            metrics::record_video_processed("webp"); // ffmpeg always emits WebP

            write_cache_atomic(&original_cache_path, &thumbnail_bytes).await?;
            thumbnail_bytes
        } else {
            // Image hash: fetch bytes from blossom servers
            let bytes = fetch_from_servers(&state.app.http, &servers, hash, ext).await?;

            if bytes.len() > state.app.cfg.max_image_bytes {
                metrics::record_processing_error("image_too_large");
                return Err(SvcError::BadRequest("image too large"));
            }

            metrics::record_bytes_downloaded("blossom", bytes.len());

            write_cache_atomic(&original_cache_path, &bytes).await?;
            bytes.to_vec()
        }
    };

    // Decode image (thumbnail bytes from ffmpeg are WebP; image bytes use guessed format)
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

    let out_fmt_str = match dirs.out_fmt {
        OutFmt::Jpeg => "jpeg",
        OutFmt::Png => "png",
        OutFmt::Webp => "webp",
        OutFmt::Avif => "avif",
    };
    metrics::record_image_processed(out_fmt_str);
    metrics::record_bytes_served(mime, encoded.len());

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

    let duration = start_time.elapsed().as_secs_f64();
    metrics::observe_http_duration("/thumb", "GET", duration);
    metrics::record_http_request("/thumb", "GET", 200);

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

    // Parse resize directive.
    // Priority: explicit `rs` param > `width`/`height` > default 480×480 fit.
    let resize = if let Some(ref rs) = params.resize {
        parse_resize_from_query(rs)?
    } else {
        let w = params.width.unwrap_or(480);
        let h = params.height.unwrap_or(480);
        Resize {
            mode: ResizeMode::Fit,
            w,
            h,
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
    if let Some(w) = params.width {
        parts.push(format!("width={}", w));
    }
    if let Some(h) = params.height {
        parts.push(format!("height={}", h));
    }

    parts.join("&")
}

/// Fetch source image from URL, with Blossom fallback when the primary fails.
async fn fetch_source(state: &AppState, src_url: &str) -> Result<Bytes, SvcError> {
    if !(src_url.starts_with("http://") || src_url.starts_with("https://")) {
        return Err(SvcError::BadRequest("unsupported source scheme"));
    }

    // Try primary URL
    match state.http.get(src_url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let bytes = resp.bytes().await?;
            tracing::debug!("primary fetch succeeded: {} ({} bytes)", src_url, bytes.len());
            return Ok(bytes);
        }
        Ok(resp) => {
            tracing::debug!("primary fetch returned {}: {}", resp.status(), src_url);
        }
        Err(e) => {
            tracing::debug!("primary fetch error for {}: {:?}", src_url, e);
        }
    }

    // Blossom fallback: only when the URL has the SHA-256 filename format
    // and is not from a re-encoding server
    if is_blossom_url(src_url) {
        if let Some((hash, ext)) = extract_blossom_hash(src_url) {
            tracing::debug!(
                "blossom fallback: trying {} servers for {}.{}",
                state.cfg.blossom_fallback_servers.len(),
                hash,
                ext
            );
            return fetch_from_servers(&state.http, &state.cfg.blossom_fallback_servers, hash, ext).await;
        }
    }

    Err(SvcError::UpstreamError(404))
}

