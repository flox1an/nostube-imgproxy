use axum::{
    body::Body,
    error_handling::HandleErrorLayer,
    extract::{ConnectInfo, MatchedPath, OriginalUri, Path as AxPath, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use axum_extra::extract::Query;
use bytes::Bytes;
use serde::Deserialize;
use std::{net::IpAddr, path::Path, sync::Arc, time::Instant};
use tower::{
    limit::GlobalConcurrencyLimitLayer, load_shed::LoadShedLayer, BoxError, ServiceBuilder,
};
use tower_http::{
    catch_panic::CatchPanicLayer,
    cors::{Any, CorsLayer},
    timeout::TimeoutLayer,
    trace::TraceLayer,
};

use crate::{
    blossom::{combine_server_lists, fetch_blob, parse_blossom_filename, BlossomState},
    cache::{
        cache_path_for, derivative_cache_key, fresh_response_headers, original_cache_path_for,
        try_read_original_cache, try_serve_cache, write_cache_atomic, ClientCachePolicy,
    },
    config::AppState,
    cpu::CpuPool,
    error::SvcError,
    fetch::read_body_capped,
    metrics,
    network_policy::validate_untrusted_url,
    preset::Preset,
    ratelimit::MediaRateLimiters,
    signing::signature_error,
    singleflight::SingleFlight,
    thumbnail::{extract_video_thumbnail, is_video_url, ThumbnailState},
    transform::{
        parse_resize_directive, parse_rest, process_image, Directives, OutFmt, Resize, ResizeMode,
    },
};

/// Combined state for image and video processing
#[derive(Clone)]
pub struct CombinedState {
    pub app: AppState,
    pub thumbnail: Arc<ThumbnailState>,
    pub blossom: Arc<BlossomState>,
    /// Bounded off-runtime executor for decode/resize/encode.
    pub cpu: CpuPool,
    /// Collapses concurrent misses for the same derivative into one job.
    pub inflight: Arc<SingleFlight>,
    /// Three-tier per-IP flood guard: general requests, image-generation
    /// cache misses, and video-generation cache misses.
    pub media_rate_limits: Arc<MediaRateLimiters>,
}

impl CombinedState {
    pub fn new(app: AppState, thumbnail: Arc<ThumbnailState>, blossom: Arc<BlossomState>) -> Self {
        let cpu = CpuPool::new(app.cfg.cpu_concurrency, app.cfg.cpu_queue_depth);
        let max_inflight = app.cfg.max_inflight_requests;
        let media_rate_limits = Arc::new(MediaRateLimiters::new(
            app.cfg.rate_ip_requests_per_min,
            app.cfg.rate_ip_image_generations_per_min,
            app.cfg.rate_ip_video_generations_per_min,
        ));
        Self {
            app,
            thumbnail,
            blossom,
            cpu,
            inflight: Arc::new(SingleFlight::new(max_inflight)),
            media_rate_limits,
        }
    }
}

/// Create the Axum router with all routes
pub fn create_router(
    state: AppState,
    thumbnail_state: Arc<ThumbnailState>,
    blossom_state: Arc<BlossomState>,
) -> Router {
    let request_timeout = state.cfg.request_timeout;
    let max_inflight = state.cfg.max_inflight_requests;
    let signed_urls_enabled = !state.cfg.url_signing_keys.is_empty();
    let allow_unsigned_urls = state.cfg.allow_unsigned_urls;
    let preset_thumbnails_enabled = state.cfg.preset_thumbnails_enabled;
    let combined = CombinedState::new(state, thumbnail_state, blossom_state);
    let mut images = Router::new();
    if signed_urls_enabled {
        images = images
            .route(
                "/v1/{key_id}/{signature}/img/{*rest}",
                get(handle_signed_image),
            )
            .route(
                "/v1/{key_id}/{signature}/thumb/{filename}",
                get(handle_signed_thumb),
            );
    } else {
        tracing::warn!("URL_SIGNING_KEYS is unset; signed media routes are disabled");
    }
    if allow_unsigned_urls {
        images = images
            .route("/insecure/{*rest}", get(handle_insecure))
            .route("/thumb/{filename}", get(handle_thumb));
    }
    if preset_thumbnails_enabled {
        images = images.route("/v1/preset/{preset}/{filename}", get(handle_preset_thumb));
    } else {
        tracing::warn!(
            "PRESET_THUMBNAILS_ENABLED=false; the unsigned preset thumbnail route is disabled"
        );
    }
    let images = images.layer(
        CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any),
    );

    Router::new()
        .merge(images)
        .route("/health", get(health_check))
        .with_state(combined)
        // Outer → inner: Trace, metrics, panic-to-500, timeout, load-shed,
        // global concurrency, handlers. Timeout includes permit waiting; shed
        // fails immediately instead of retaining an unbounded waiter queue.
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(|_: BoxError| async {
                    SvcError::Overloaded.into_response()
                }))
                .layer(TimeoutLayer::with_status_code(
                    StatusCode::REQUEST_TIMEOUT,
                    request_timeout,
                ))
                .layer(LoadShedLayer::new())
                .layer(GlobalConcurrencyLimitLayer::new(max_inflight)),
        )
        .layer(CatchPanicLayer::new())
        .layer(middleware::from_fn(record_response_metrics))
        .layer(TraceLayer::new_for_http())
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

/// Query parameters accepted by the unsigned preset thumbnail route. No
/// directive fields are accepted: `deny_unknown_fields` rejects `f`, `rs`,
/// `q`, `width`, or `height` outright, so a caller cannot smuggle a
/// directive override past the preset name. Only Blossom server-discovery
/// hints are meaningful here.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PresetQuery {
    /// Server hints — hostnames or full URLs (xs= can repeat)
    #[serde(rename = "xs", default)]
    server_hints: Vec<String>,

    /// Author pubkey (npub or hex) for kind 10063 relay lookup
    #[serde(rename = "as")]
    author_pubkey: Option<String>,
}

/// Unsigned, fixed-preset Blossom thumbnail route: `GET
/// /v1/preset/{preset}/{filename}`.
///
/// Deliberately unauthenticated and un-minted: the preset name is the only
/// server-authoritative source of output directives, so there is no open
/// value space for a client to abuse. Admission is the same per-IP tiered
/// rate limiter every other image/thumb route uses.
async fn handle_preset_thumb(
    State(state): State<CombinedState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    AxPath((preset, filename)): AxPath<(String, String)>,
    Query(params): Query<PresetQuery>,
    request_headers: HeaderMap,
) -> Result<Response, SvcError> {
    let preset = Preset::parse(&preset).ok_or(SvcError::BadRequest("unknown preset"))?;
    let hints = BlossomHints {
        server_hints: &params.server_hints,
        author_pubkey: params.author_pubkey.as_deref(),
    };
    handle_thumb_request(
        state,
        filename,
        preset.directives(),
        hints,
        request_headers,
        None,
        peer.ip(),
    )
    .await
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

/// Operator-only metrics router. It is deliberately not merged into the
/// public image router; `main` binds it only when `METRICS_BIND_ADDR` is set.
pub fn create_metrics_router() -> Router {
    Router::new().route("/metrics", get(handle_metrics))
}

/// Record the response that actually leaves the service. `MatchedPath` keeps
/// labels bounded even when attackers send arbitrary paths or query strings.
async fn record_response_metrics(request: Request, next: Next) -> Response {
    let started = Instant::now();
    let endpoint = request
        .extensions()
        .get::<MatchedPath>()
        .map_or("<unmatched>", MatchedPath::as_str)
        .to_owned();
    let method = request.method().as_str().to_owned();
    let response = next.run(request).await;
    metrics::observe_http_duration(&endpoint, &method, started.elapsed().as_secs_f64());
    metrics::record_http_request(&endpoint, &method, response.status().as_u16());
    response
}

/// Where the *original* bytes for a derivative come from.
///
/// Owned rather than borrowed so the whole production job can be handed to
/// [`SingleFlight`], which requires a `'static` future.
enum Source {
    /// An ordinary URL: fetched as an image, or FFmpeg-thumbnailed if it looks
    /// like a video.
    Direct { url: String, is_video: bool },
    /// A hash-addressed Blossom blob resolved across candidate servers.
    Blossom {
        hash: String,
        ext: Option<String>,
        servers: Vec<String>,
        discovered: Vec<String>,
        is_video: bool,
    },
}

impl Source {
    fn is_video(&self) -> bool {
        match self {
            Source::Direct { is_video, .. } | Source::Blossom { is_video, .. } => *is_video,
        }
    }

    /// Range-probed video sources cannot be proven to match their advertised
    /// SHA-256 without a full download. Never let their thumbnails enter a
    /// hash-keyed disk cache or receive a reusable entity validator.
    fn cacheable(&self) -> bool {
        !self.is_video()
    }
}

/// Serve a cached derivative if one exists, recording the cache hit.
async fn serve_cached(
    cache_path: &Path,
    mime: &str,
    request_headers: &HeaderMap,
    policy: ClientCachePolicy,
) -> Result<Option<Response>, SvcError> {
    let Some(resp) = try_serve_cache(cache_path, mime, request_headers, policy).await? else {
        return Ok(None);
    };
    metrics::record_cache_hit("processed");
    Ok(Some(resp))
}

/// Obtain the original bytes for `source`, using the original-bytes cache first.
async fn load_original(
    state: &CombinedState,
    source: &Source,
    original_cache_path: &Path,
    deadline: Instant,
) -> Result<Vec<u8>, SvcError> {
    if source.cacheable() {
        if let Some(cached) = try_read_original_cache(original_cache_path).await? {
            metrics::record_cache_hit("original");
            return Ok(cached);
        }
        metrics::record_cache_miss("original");
    }

    let cfg = &state.app.cfg;
    let bytes = match source {
        Source::Direct {
            url,
            is_video: true,
        } => {
            let thumbnail = extract_video_thumbnail(
                url,
                &state.thumbnail.ffmpeg_semaphore,
                &state.app.http,
                &[],
                &[],
                None,
                None,
                cfg.max_blob_candidates,
                cfg.max_video_probe_bytes,
                cfg.max_image_bytes,
                deadline,
                cfg.ffmpeg_timeout,
            )
            .await?;
            thumbnail
        }
        Source::Direct { url, .. } => {
            let bytes = fetch_source(&state.app, url).await?;
            metrics::record_bytes_downloaded("image", bytes.len());
            bytes.to_vec()
        }
        Source::Blossom {
            hash,
            ext,
            servers,
            discovered,
            is_video: true,
        } => {
            let primary = servers
                .first()
                .map(|server| blossom_blob_url(server, hash, ext.as_deref()))
                .or_else(|| discovered.first().cloned())
                .ok_or(SvcError::BadRequest(
                    "no servers available for video thumbnail",
                ))?;
            let thumbnail = extract_video_thumbnail(
                &primary,
                &state.thumbnail.ffmpeg_semaphore,
                &state.app.http,
                servers,
                discovered,
                Some(state.blossom.candidate_failure_cache()),
                Some(hash),
                cfg.max_blob_candidates,
                cfg.max_video_probe_bytes,
                cfg.max_image_bytes,
                deadline,
                cfg.ffmpeg_timeout,
            )
            .await?;
            thumbnail
        }
        Source::Blossom {
            hash,
            ext,
            servers,
            discovered,
            ..
        } => {
            let bytes = fetch_blob(
                &state.app.http,
                state.blossom.candidate_failure_cache(),
                servers,
                discovered,
                hash,
                ext.as_deref(),
                deadline,
                cfg.max_image_bytes,
                cfg.max_blob_candidates,
                cfg.fetch_timeout,
            )
            .await?;
            metrics::record_bytes_downloaded("blossom", bytes.len());
            bytes.to_vec()
        }
    };

    Ok(bytes)
}

/// Produce one derivative from scratch and persist it.
///
/// Runs as the body of a single-flight leader, so exactly one of these executes
/// per cache key no matter how many requests arrive at once.
async fn produce_derivative(
    state: CombinedState,
    source: Source,
    dirs: Directives,
    cache_path: std::path::PathBuf,
    original_cache_path: std::path::PathBuf,
    deadline: Instant,
) -> Result<Bytes, SvcError> {
    let original = load_original(&state, &source, &original_cache_path, deadline).await?;

    let limits = state.app.cfg.decode_limits();
    let out_fmt_str = dirs.out_fmt.label();
    // Decode/resize/encode is the only CPU-heavy step; it must never run on an
    // async worker or a few concurrent encodes stall the whole reactor. The
    // closure moves `original` in and hands it back beside the encoded output
    // so the original-cache write can wait for the decode to validate the
    // bytes, without cloning a payload that can be tens of megabytes.
    let (encoded, original) = state
        .cpu
        .run(move || {
            let encoded = process_image(&original, &dirs, limits)?;
            Ok::<_, SvcError>((encoded, original))
        })
        .await??;

    if source.is_video() {
        metrics::record_video_processed(out_fmt_str);
    } else {
        metrics::record_image_processed(out_fmt_str);
    }

    if source.cacheable() {
        // The original is persisted only after decode/resize/encode has proven
        // it is an image. Range-probed videos deliberately skip this: their
        // source hash cannot be verified without a full download.
        write_cache_atomic(&cache_path, &encoded).await?;
        write_cache_atomic(&original_cache_path, &original).await?;
    }
    Ok(Bytes::from(encoded))
}

/// Build the response for a derivative that was produced rather than cached.
fn fresh_response(
    encoded: Bytes,
    mime: &str,
    cache_path: &Path,
    coalesced: bool,
    policy: ClientCachePolicy,
) -> Response {
    metrics::record_bytes_served(mime, encoded.len());
    let mut resp = Response::new(Body::from(encoded));
    *resp.status_mut() = StatusCode::OK;
    let cache_state = if coalesced { "coalesced" } else { "miss" };
    fresh_response_headers(resp.headers_mut(), mime, cache_path, cache_state, policy);
    resp
}

fn blossom_blob_url(server: &str, hash: &str, ext: Option<&str>) -> String {
    match ext {
        Some(ext) => format!("{}/{hash}.{ext}", server.trim_end_matches('/')),
        None => format!("{}/{hash}", server.trim_end_matches('/')),
    }
}

/// Legacy unsigned `/insecure/{*}` route. It remains available only while
/// `ALLOW_UNSIGNED_URLS=true` during the signed-URL migration.
async fn handle_insecure(
    State(state): State<CombinedState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    AxPath(rest): AxPath<String>,
    request_headers: HeaderMap,
) -> Result<Response, SvcError> {
    handle_image_request(state, rest, request_headers, None, peer.ip()).await
}

/// Versioned signed direct-media route.
async fn handle_signed_image(
    State(state): State<CombinedState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    OriginalUri(uri): OriginalUri,
    AxPath((key_id, signature, rest)): AxPath<(String, String, String)>,
    request_headers: HeaderMap,
) -> Result<Response, SvcError> {
    let expiry = verify_signed_request(&state, &uri, &key_id, &signature, "/img/")?;
    handle_image_request(state, rest, request_headers, expiry, peer.ip()).await
}

async fn handle_image_request(
    state: CombinedState,
    rest: String,
    request_headers: HeaderMap,
    signed_expiry: Option<std::time::SystemTime>,
    peer_ip: IpAddr,
) -> Result<Response, SvcError> {
    // Parse and validate before any cache lookup. The request URL is untrusted
    // and must never become an FFmpeg or HTTP target on this server.
    let (dirs, src_url) = parse_rest(&rest)?;
    dirs.resize.validate(state.app.cfg.max_image_dimension)?;
    validate_untrusted_url(&src_url)?;

    state
        .media_rate_limits
        .admit_request(peer_ip)
        .inspect_err(|_| metrics::record_rate_limit_rejection("request"))?;

    // Signed and legacy direct media deliberately share this namespace: access
    // control changes who may request a derivative, not its output bytes.
    let is_video = is_video_url(&src_url);
    let cache_key = derivative_cache_key("insecure", &src_url, &dirs);
    let cache_path = cache_path_for(&state.app.cfg, &cache_key, &dirs.out_fmt);
    let mime = dirs.out_fmt.mime_type();
    let policy = signed_expiry
        .map(ClientCachePolicy::ExpiresAt)
        .unwrap_or(ClientCachePolicy::ShortLived);

    if !is_video {
        if let Some(resp) = serve_cached(&cache_path, mime, &request_headers, policy).await? {
            return Ok(resp);
        }
    }
    metrics::record_cache_miss("processed");
    state
        .media_rate_limits
        .admit_generation(peer_ip, is_video)
        .inspect_err(|_| {
            metrics::record_rate_limit_rejection(if is_video {
                "video_generation"
            } else {
                "image_generation"
            })
        })?;
    let original_cache_path = original_cache_path_for(&state.app.cfg, &src_url);
    let deadline = Instant::now() + state.app.cfg.fetch_timeout;
    let source = Source::Direct {
        is_video,
        url: src_url,
    };

    let inflight = Arc::clone(&state.inflight);
    let outcome = {
        let state = state.clone();
        let cache_path = cache_path.clone();
        let original_cache_path = original_cache_path.clone();
        inflight
            .run(&cache_key, move || {
                produce_derivative(
                    state,
                    source,
                    dirs,
                    cache_path,
                    original_cache_path,
                    deadline,
                )
            })
            .await?
    };

    let mut resp = fresh_response(outcome.bytes, mime, &cache_path, outcome.coalesced, policy);
    if is_video {
        resp.headers_mut().remove(header::ETAG);
    }
    Ok(resp)
}

/// Legacy unsigned `/thumb/{filename}` route. It remains available only while
/// `ALLOW_UNSIGNED_URLS=true` during the signed-URL migration.
async fn handle_thumb(
    State(state): State<CombinedState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    AxPath(filename): AxPath<String>,
    Query(params): Query<ThumbQuery>,
    request_headers: HeaderMap,
) -> Result<Response, SvcError> {
    let dirs = parse_thumb_params(&params, state.app.cfg.max_image_dimension)?;
    let hints = BlossomHints {
        server_hints: &params.server_hints,
        author_pubkey: params.author_pubkey.as_deref(),
    };
    handle_thumb_request(
        state,
        filename,
        dirs,
        hints,
        request_headers,
        None,
        peer.ip(),
    )
    .await
}

/// Versioned signed Blossom thumbnail route.
async fn handle_signed_thumb(
    State(state): State<CombinedState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    OriginalUri(uri): OriginalUri,
    AxPath((key_id, signature, filename)): AxPath<(String, String, String)>,
    Query(params): Query<ThumbQuery>,
    request_headers: HeaderMap,
) -> Result<Response, SvcError> {
    let expiry = verify_signed_request(&state, &uri, &key_id, &signature, "/thumb/")?;
    let dirs = parse_thumb_params(&params, state.app.cfg.max_image_dimension)?;
    let hints = BlossomHints {
        server_hints: &params.server_hints,
        author_pubkey: params.author_pubkey.as_deref(),
    };
    handle_thumb_request(
        state,
        filename,
        dirs,
        hints,
        request_headers,
        expiry,
        peer.ip(),
    )
    .await
}

/// Which Blossom servers to try for a hash, gathered from optional request
/// hints. Bundled so `handle_thumb_request` stays under clippy's argument
/// count lint.
struct BlossomHints<'a> {
    server_hints: &'a [String],
    author_pubkey: Option<&'a str>,
}

async fn handle_thumb_request(
    state: CombinedState,
    filename: String,
    dirs: Directives,
    hints: BlossomHints<'_>,
    request_headers: HeaderMap,
    signed_expiry: Option<std::time::SystemTime>,
    peer_ip: IpAddr,
) -> Result<Response, SvcError> {
    // Accept both `<sha256>` and `<sha256>.<ext>` and canonicalize the hash
    // before using it as an upstream path or cache key.
    let (hash, ext) =
        parse_blossom_filename(&filename).ok_or(SvcError::BadRequest("invalid SHA256 filename"))?;
    let hash = hash.to_ascii_lowercase();
    let ext = ext.map(str::to_ascii_lowercase);
    let blob_name = match &ext {
        Some(ext) => format!("{hash}.{ext}"),
        None => hash.clone(),
    };
    let is_video = ext
        .as_deref()
        .is_some_and(|extension| is_video_url(&format!("{hash}.{extension}")));

    state
        .media_rate_limits
        .admit_request(peer_ip)
        .inspect_err(|_| metrics::record_rate_limit_rejection("request"))?;

    // Build cache key from the canonical blob name and request parameters.
    let cache_key = derivative_cache_key("thumb", &blob_name, &dirs);
    let cache_path = cache_path_for(&state.app.cfg, &cache_key, &dirs.out_fmt);
    let mime = dirs.out_fmt.mime_type();
    let policy = signed_expiry
        .map(ClientCachePolicy::ExpiresAt)
        .unwrap_or(if is_video {
            ClientCachePolicy::ShortLived
        } else {
            ClientCachePolicy::Immutable
        });

    if !is_video {
        if let Some(resp) = serve_cached(&cache_path, mime, &request_headers, policy).await? {
            return Ok(resp);
        }
    }
    metrics::record_cache_miss("processed");
    state
        .media_rate_limits
        .admit_generation(peer_ip, is_video)
        .inspect_err(|_| {
            metrics::record_rate_limit_rejection(if is_video {
                "video_generation"
            } else {
                "image_generation"
            })
        })?;

    // Get author servers if pubkey provided
    let author_servers = if let Some(pubkey) = hints.author_pubkey {
        match state.blossom.get_author_servers(pubkey).await {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(
                    "Failed to fetch author servers for pubkey {}: {}",
                    pubkey,
                    e
                );
                None
            }
        }
    } else {
        None
    };

    // Combine servers: xs (highest priority) -> as -> fallback
    let servers = combine_server_lists(
        if hints.server_hints.is_empty() {
            None
        } else {
            Some(hints.server_hints)
        },
        author_servers.as_deref(),
        &state.app.cfg.blossom_fallback_servers,
        state.app.cfg.max_server_hints,
    );

    let discovered = match state.blossom.discover_blob_urls(&hash).await {
        Ok(urls) => urls,
        Err(error) => {
            tracing::warn!(
                "Failed to discover NIP-94 locations for {}: {}",
                hash,
                error
            );
            Vec::new()
        }
    };
    let deadline = Instant::now() + state.app.cfg.blossom_failover_timeout;

    tracing::debug!(
        "Resolved {} server and {} discovered candidates for {}",
        servers.len(),
        discovered.len(),
        blob_name
    );

    let original_cache_path = original_cache_path_for(&state.app.cfg, &blob_name);
    let source = Source::Blossom {
        hash,
        ext,
        servers,
        discovered,
        is_video,
    };

    let inflight = Arc::clone(&state.inflight);
    let outcome = {
        let state = state.clone();
        let cache_path = cache_path.clone();
        let original_cache_path = original_cache_path.clone();
        inflight
            .run(&cache_key, move || {
                produce_derivative(
                    state,
                    source,
                    dirs,
                    cache_path,
                    original_cache_path,
                    deadline,
                )
            })
            .await?
    };

    let mut resp = fresh_response(outcome.bytes, mime, &cache_path, outcome.coalesced, policy);
    if is_video {
        resp.headers_mut().remove(header::ETAG);
    }
    Ok(resp)
}

fn verify_signed_request(
    state: &CombinedState,
    uri: &http::Uri,
    key_id: &str,
    signature: &str,
    expected_path_prefix: &str,
) -> Result<Option<std::time::SystemTime>, SvcError> {
    let raw = uri
        .path_and_query()
        .map(|path_and_query| path_and_query.as_str())
        .ok_or(SvcError::Forbidden("invalid signed URL"))?;
    let prefix = format!("/v1/{key_id}/{signature}");
    let path_and_query = raw
        .strip_prefix(&prefix)
        .filter(|value| value.starts_with(expected_path_prefix))
        .ok_or(SvcError::Forbidden("invalid signed URL"))?;

    match state.app.cfg.url_signing_keys.verify(
        key_id,
        signature,
        path_and_query,
        state.app.cfg.require_signed_url_expiry,
        std::time::SystemTime::now(),
    ) {
        Ok(verified) => {
            metrics::record_signature_verification("ok");
            Ok(verified.expires_at)
        }
        Err(failure) => {
            metrics::record_signature_verification(failure.as_str());
            Err(signature_error(failure))
        }
    }
}

/// Parse thumb query parameters into Directives
fn parse_thumb_params(params: &ThumbQuery, max_dimension: u32) -> Result<Directives, SvcError> {
    // Parse output format
    let out_fmt = if let Some(fmt) = &params.format {
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

    // Parse quality. `ravif` asserts a floor of 1, so 0 is not a legal request.
    let quality = params.quality.unwrap_or(82);
    if !(1..=100).contains(&quality) {
        return Err(SvcError::BadRequest("quality must be 1-100"));
    }

    // Parse resize directive.
    // Priority: explicit `rs` param > `width`/`height` > default 480×480 fit.
    let resize = if let Some(rs) = &params.resize {
        parse_resize_directive(rs)?
    } else {
        let w = params.width.unwrap_or(480);
        let h = params.height.unwrap_or(480);
        Resize {
            mode: ResizeMode::Fit,
            w,
            h,
        }
    };

    // A zero-by-zero box means "keep the source size", which turns an arbitrary
    // upstream image into an unbounded output. Require an explicit dimension.
    if resize.w == 0 && resize.h == 0 {
        return Err(SvcError::BadRequest("at least one dimension required"));
    }
    resize.validate(max_dimension)?;

    Ok(Directives {
        out_fmt,
        quality,
        resize,
    })
}

/// Fetch a non-Blossom source URL, bounded by `max_image_bytes`.
///
/// Hash-addressed Blossom media is resolved exclusively through `/thumb`, where
/// request hints and the author server list participate in candidate selection.
async fn fetch_source(state: &AppState, src_url: &str) -> Result<Bytes, SvcError> {
    validate_untrusted_url(src_url)?;

    let response = state.http.get(src_url).send().await?;
    if !response.status().is_success() {
        tracing::debug!("primary fetch returned {}: {}", response.status(), src_url);
        return Err(SvcError::UpstreamError(response.status().as_u16()));
    }

    let bytes = read_body_capped(response, state.cfg.max_image_bytes).await?;
    tracing::debug!(
        "primary fetch succeeded: {} ({} bytes)",
        src_url,
        bytes.len()
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_probed_video_source_is_never_cacheable() {
        let video = Source::Direct {
            url: "https://cdn.example/video.mp4".into(),
            is_video: true,
        };
        let image = Source::Direct {
            url: "https://cdn.example/image.png".into(),
            is_video: false,
        };

        assert!(!video.cacheable());
        assert!(image.cacheable());
    }
}
