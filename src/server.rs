use axum::{
    body::Body,
    error_handling::HandleErrorLayer,
    extract::{
        ConnectInfo, FromRequestParts, MatchedPath, OriginalUri, Path as AxPath, Request, State,
    },
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
    audio,
    blossom::{combine_server_lists, fetch_blob, parse_blossom_filename, BlossomState},
    cache::{
        cache_path_for, derivative_cache_key, fresh_response_headers, original_cache_path_for,
        try_read_original_cache, try_serve_cache, write_cache_atomic, ClientCachePolicy,
        INSECURE_ROUTE, THUMB_ROUTE,
    },
    config::AppState,
    cpu::CpuPool,
    error::SvcError,
    fetch::read_body_capped,
    hls, metrics,
    network_policy::validate_untrusted_url,
    preset::Preset,
    ratelimit::MediaRateLimiters,
    signing::signature_error,
    singleflight::SingleFlight,
    thumbnail::{
        extract_thumbnail_from_verified_playlist, extract_video_thumbnail, is_video_url,
        ThumbnailState,
    },
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
    pub inflight: Arc<SingleFlight<Derivative>>,
    /// Three-tier per-IP flood guard: general requests, image-generation
    /// cache misses, and video-generation cache misses.
    pub media_rate_limits: Arc<MediaRateLimiters>,
}

/// A freshly produced derivative. `cache_path` is set only when it was
/// written to the disk cache, which is the only case that may carry a stable
/// ETag and (on `/thumb`) be pinned `immutable`.
#[derive(Clone)]
pub struct Derivative {
    pub bytes: Bytes,
    pub cache_path: Option<std::path::PathBuf>,
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
        .route("/metrics", get(handle_public_metrics))
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
    ClientIp(client_ip): ClientIp,
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
        client_ip,
    )
    .await
}

/// Rate-limit identity of the caller: the TCP peer, or behind a trusted
/// reverse proxy the client it forwarded for (see [`crate::ratelimit::client_ip`]).
struct ClientIp(IpAddr);

impl FromRequestParts<CombinedState> for ClientIp {
    type Rejection =
        <ConnectInfo<std::net::SocketAddr> as FromRequestParts<CombinedState>>::Rejection;

    async fn from_request_parts(
        parts: &mut http::request::Parts,
        state: &CombinedState,
    ) -> Result<Self, Self::Rejection> {
        let ConnectInfo(peer) =
            ConnectInfo::<std::net::SocketAddr>::from_request_parts(parts, state).await?;
        Ok(Self(crate::ratelimit::client_ip(
            peer.ip(),
            &parts.headers,
            &state.app.cfg.trusted_proxies,
        )))
    }
}

/// Simple health check endpoint
async fn health_check() -> &'static str {
    "OK"
}

/// Public `/metrics` on the main router, almond-style: disabled (404) unless
/// `METRICS_BEARER_TOKEN` is configured; a missing or wrong bearer is a 401.
/// The token-free scrape path remains the `METRICS_BIND_ADDR` listener, which
/// is a management-network interface by construction.
async fn handle_public_metrics(
    State(state): State<CombinedState>,
    request_headers: HeaderMap,
) -> Response {
    let Some(token) = state.app.cfg.metrics_bearer_token.as_deref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Compare digests, not the secret itself: `==` on the raw token exits at
    // the first differing byte and would leak a matching prefix by timing.
    let authorized = request_headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|provided| {
            use sha2::{Digest, Sha256};
            Sha256::digest(provided.as_bytes()) == Sha256::digest(token.as_bytes())
        });
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match metrics::encode_metrics() {
        Ok(body) => (
            [(
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )],
            body,
        )
            .into_response(),
        Err(error) => {
            tracing::error!(error = %error, "failed to encode metrics");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
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

/// Record the response that actually leaves the service. `MatchedPath` and
/// [`method_label`] keep labels bounded even when attackers send arbitrary
/// paths, query strings or extension methods.
async fn record_response_metrics(request: Request, next: Next) -> Response {
    let started = Instant::now();
    let endpoint = request
        .extensions()
        .get::<MatchedPath>()
        .map_or("<unmatched>", MatchedPath::as_str)
        .to_owned();
    let method = method_label(request.method());
    let response = next.run(request).await;
    metrics::observe_http_duration(&endpoint, method, started.elapsed().as_secs_f64());
    metrics::record_http_request(&endpoint, method, response.status().as_u16());
    response
}

/// Fixed label set: hyper accepts any extension-method token, and each new
/// label value is a Prometheus series that is never freed.
fn method_label(method: &axum::http::Method) -> &'static str {
    use axum::http::Method;
    match *method {
        Method::GET => "GET",
        Method::HEAD => "HEAD",
        Method::OPTIONS => "OPTIONS",
        Method::POST => "POST",
        _ => "OTHER",
    }
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

    /// Cache identities whose entries this source may reuse, in preference
    /// order: the URL for `/insecure`, the blob name for a hash-verified
    /// Blossom blob, one `blob@origin` per candidate server for a
    /// range-probed Blossom video (see [`video_identities`]).
    fn cache_identities(&self) -> Vec<String> {
        match self {
            Source::Direct { url, .. } => vec![url.clone()],
            Source::Blossom {
                hash,
                ext,
                servers,
                is_video,
                ..
            } => {
                let name = blob_name(hash, ext.as_deref());
                if *is_video {
                    video_identities(&name, servers)
                } else {
                    vec![name]
                }
            }
        }
    }
}

fn blob_name(hash: &str, ext: Option<&str>) -> String {
    match ext {
        Some(ext) => format!("{hash}.{ext}"),
        None => hash.to_owned(),
    }
}

/// `scheme://host[:port]` of a server base URL or candidate URL.
fn url_origin(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .map(|url| url.origin().ascii_serialization())
}

/// Cache identities of a range-probed Blossom video, one per distinct origin
/// in `servers` (request order).
///
/// The frame comes from a range probe, never from a hash check of the whole
/// blob (that would mean downloading every video in full), so it is only
/// trusted as "what this origin served for this blob". Keying by origin keeps
/// a hostile `xs=` origin from poisoning anyone else: its entry is only ever
/// looked up by requests that would ask that origin themselves.
fn video_identities(blob_name: &str, servers: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    servers
        .iter()
        .filter_map(|server| url_origin(server))
        .filter(|origin| seen.insert(origin.clone()))
        .map(|origin| format!("{blob_name}@{origin}"))
        .collect()
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

/// Obtain the original bytes for `source`, using the original-bytes cache
/// first. Also returns the cache identity those bytes may be persisted
/// under, or `None` when they must not be cached:
///
/// - `/insecure`: the URL itself, image or video.
/// - Blossom image: the blob name, after the full-body hash check in
///   `fetch_blob`.
/// - Blossom video: `blob@origin` of the candidate that served the probe, if
///   that origin is one of the request's servers. A NIP-94-discovered origin
///   (anyone may publish those) is used but never persisted.
/// - HLS sniffed behind a non-video name: never (unverified segments behind a
///   lookup that only knows the hint-free blob name).
async fn load_original(
    state: &CombinedState,
    source: &Source,
    route: &'static str,
    deadline: Instant,
) -> Result<(Vec<u8>, Option<String>), SvcError> {
    let cfg = &state.app.cfg;
    for identity in source.cache_identities() {
        let path = original_cache_path_for(cfg, route, &identity);
        if let Some(cached) = try_read_original_cache(&path).await? {
            metrics::record_cache_hit("original");
            return Ok((cached, Some(identity)));
        }
    }
    metrics::record_cache_miss("original");

    let loaded = match source {
        Source::Direct {
            url,
            is_video: true,
        } => {
            let (thumbnail, _) = extract_video_thumbnail(
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
            (thumbnail, Some(url.clone()))
        }
        Source::Direct { url, .. } => {
            let bytes = fetch_source(&state.app, url).await?;
            metrics::record_bytes_downloaded("image", bytes.len());
            (bytes.to_vec(), Some(url.clone()))
        }
        Source::Blossom {
            hash,
            ext,
            servers,
            discovered,
            is_video: true,
        } => {
            // Range-probe only: a thumbnail needs the container index and a
            // few seconds near one keyframe, never the whole file.
            let primary = servers
                .first()
                .map(|server| blossom_blob_url(server, hash, ext.as_deref()))
                .or_else(|| discovered.first().cloned())
                .ok_or(SvcError::BadRequest(
                    "no servers available for video thumbnail",
                ))?;
            let (thumbnail, served_by) = extract_video_thumbnail(
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
            let name = blob_name(hash, ext.as_deref());
            let identity = url_origin(&served_by)
                .map(|origin| format!("{name}@{origin}"))
                .filter(|identity| source.cache_identities().contains(identity));
            (thumbnail, identity)
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
            // HLS playlists are published under arbitrary names (real-world
            // Blossom blobs use `.txt`), so the extension cannot be trusted:
            // sniff the verified bytes. The playlist itself is hash-verified,
            // but its segment bytes are not, so the thumbnail is not cached.
            if hls::is_hls_playlist(&bytes) {
                tracing::info!(hash = %hash, "HLS playlist detected via content sniff");
                let primary = servers
                    .first()
                    .map(|server| blossom_blob_url(server, hash, ext.as_deref()))
                    .ok_or(SvcError::BadRequest(
                        "no servers available for video thumbnail",
                    ))?;
                let thumbnail = extract_thumbnail_from_verified_playlist(
                    &bytes,
                    &primary,
                    &state.thumbnail.ffmpeg_semaphore,
                    &state.app.http,
                    cfg.max_video_probe_bytes,
                    cfg.max_image_bytes,
                    Instant::now() + cfg.video_deadline,
                    cfg.ffmpeg_timeout,
                )
                .await?;
                return Ok((thumbnail, None));
            }
            (bytes.to_vec(), Some(blob_name(hash, ext.as_deref())))
        }
    };

    Ok(loaded)
}

/// Produce one derivative from scratch and persist it when its source allows.
///
/// Runs as the body of a single-flight leader, so exactly one of these executes
/// per flight key no matter how many requests arrive at once.
async fn produce_derivative(
    state: CombinedState,
    source: Source,
    dirs: Directives,
    route: &'static str,
    deadline: Instant,
) -> Result<Derivative, SvcError> {
    let (original, identity) = load_original(&state, &source, route, deadline).await?;
    let cfg = &state.app.cfg;
    let paths = identity.map(|identity| {
        let key = derivative_cache_key(route, &identity, &dirs);
        (
            cache_path_for(cfg, route, &key, &dirs.out_fmt),
            original_cache_path_for(cfg, route, &identity),
        )
    });

    let limits = cfg.decode_limits();
    let out_fmt_str = dirs.out_fmt.label();
    // Decode/resize/encode is the only CPU-heavy step; it must never run on an
    // async worker or a few concurrent encodes stall the whole reactor. The
    // closure moves `original` in and hands it back beside the encoded output
    // so the original-cache write can wait for the decode to validate the
    // bytes, without cloning a payload that can be tens of megabytes.
    let (encoded, original) = state
        .cpu
        .run(move || {
            // An audio blob carries no pixels; substitute its embedded cover
            // art and let the normal decode → resize → encode path take over.
            // The *audio* bytes stay `original` so the original-bytes cache
            // keeps holding what was fetched, and later requests re-extract
            // from them instead of re-downloading.
            let cover;
            let image_bytes: &[u8] = if audio::is_audio(&original) {
                cover = audio::extract_cover_art(&original)?;
                &cover
            } else {
                &original
            };
            let encoded = process_image(image_bytes, &dirs, limits)?;
            Ok::<_, SvcError>((encoded, original))
        })
        .await??;

    if source.is_video() {
        metrics::record_video_processed(out_fmt_str);
    } else {
        metrics::record_image_processed(out_fmt_str);
    }

    let cache_path = match paths {
        Some((cache_path, original_cache_path)) => {
            write_cache_atomic(&cache_path, &encoded).await?;
            write_cache_atomic(&original_cache_path, &original).await?;
            Some(cache_path)
        }
        None => None,
    };
    Ok(Derivative {
        bytes: Bytes::from(encoded),
        cache_path,
    })
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
    ClientIp(client_ip): ClientIp,
    AxPath(rest): AxPath<String>,
    request_headers: HeaderMap,
) -> Result<Response, SvcError> {
    handle_image_request(state, rest, request_headers, None, client_ip).await
}

/// Versioned signed direct-media route.
async fn handle_signed_image(
    State(state): State<CombinedState>,
    ClientIp(client_ip): ClientIp,
    OriginalUri(uri): OriginalUri,
    AxPath((key_id, signature, rest)): AxPath<(String, String, String)>,
    request_headers: HeaderMap,
) -> Result<Response, SvcError> {
    let expiry = verify_signed_request(&state, &uri, &key_id, &signature, "/img/")?;
    handle_image_request(state, rest, request_headers, expiry, client_ip).await
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

    // Signed and legacy direct media deliberately share this namespace: access
    // control changes who may request a derivative, not its output bytes.
    let is_video = is_video_url(&src_url);
    let cache_key = derivative_cache_key(INSECURE_ROUTE, &src_url, &dirs);
    let cache_path = cache_path_for(&state.app.cfg, INSECURE_ROUTE, &cache_key, &dirs.out_fmt);
    let mime = dirs.out_fmt.mime_type();
    let policy = signed_expiry
        .map(ClientCachePolicy::ExpiresAt)
        .unwrap_or(ClientCachePolicy::ShortLived);

    if let Some(resp) = serve_cached(&cache_path, mime, &request_headers, policy).await? {
        return Ok(resp);
    }
    state
        .media_rate_limits
        .admit_request(peer_ip)
        .inspect_err(|_| metrics::record_rate_limit_rejection("request"))?;

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
    // A video needs several range-probe round trips where an image needs
    // one; reusing `fetch_timeout` for both let video silently inherit a
    // budget sized for the cheaper case.
    let deadline = Instant::now()
        + if is_video {
            state.app.cfg.video_deadline
        } else {
            state.app.cfg.fetch_timeout
        };
    let source = Source::Direct {
        is_video,
        url: src_url,
    };

    let inflight = Arc::clone(&state.inflight);
    let outcome = {
        let state = state.clone();
        inflight
            .run(&cache_key, move || {
                produce_derivative(state, source, dirs, INSECURE_ROUTE, deadline)
            })
            .await?
    };

    let persisted_path = outcome.value.cache_path.clone();
    let mut resp = fresh_response(
        outcome.value.bytes,
        mime,
        persisted_path.as_deref().unwrap_or(&cache_path),
        outcome.coalesced,
        policy,
    );
    if persisted_path.is_none() {
        resp.headers_mut().remove(header::ETAG);
    }
    Ok(resp)
}

/// Legacy unsigned `/thumb/{filename}` route. It remains available only while
/// `ALLOW_UNSIGNED_URLS=true` during the signed-URL migration.
async fn handle_thumb(
    State(state): State<CombinedState>,
    ClientIp(client_ip): ClientIp,
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
        client_ip,
    )
    .await
}

/// Versioned signed Blossom thumbnail route.
async fn handle_signed_thumb(
    State(state): State<CombinedState>,
    ClientIp(client_ip): ClientIp,
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
        client_ip,
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
    let blob_name = blob_name(&hash, ext.as_deref());
    let ext_is_video = ext
        .as_deref()
        .is_some_and(|extension| is_video_url(&format!("{hash}.{extension}")));

    // Images (hash-verified) live under the hint-free blob name. Videos live
    // per origin and need the request's server list first, see below.
    let cache_key = derivative_cache_key(THUMB_ROUTE, &blob_name, &dirs);
    let cache_path = cache_path_for(&state.app.cfg, THUMB_ROUTE, &cache_key, &dirs.out_fmt);
    let mime = dirs.out_fmt.mime_type();
    // Entries under `thumb/` are hash-verified images or a video frame bound
    // to the origin that served it; both are stable for their key.
    let hit_policy = signed_expiry
        .map(ClientCachePolicy::ExpiresAt)
        .unwrap_or(ClientCachePolicy::Immutable);

    if !ext_is_video {
        if let Some(resp) = serve_cached(&cache_path, mime, &request_headers, hit_policy).await? {
            return Ok(resp);
        }
    }
    state
        .media_rate_limits
        .admit_request(peer_ip)
        .inspect_err(|_| metrics::record_rate_limit_rejection("request"))?;

    // Get author servers if pubkey provided
    let author_servers = if let Some(pubkey) = hints.author_pubkey {
        match state.blossom.get_author_servers(pubkey).await {
            Ok(s) => Some(s),
            Err(e) => {
                // `as=` is attacker-controlled: never log it raw. The error
                // carries a length-capped copy; Debug escapes line breaks.
                tracing::warn!(error = ?e, "failed to fetch author servers");
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

    if ext_is_video {
        if let Some(resp) = serve_cached_video(
            &state,
            &blob_name,
            &dirs,
            &servers,
            &request_headers,
            hit_policy,
        )
        .await?
        {
            return Ok(resp);
        }
    }

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

    // A caller that requests a bare hash (no extension) gives us no way to
    // tell video from image up front. NIP-94 discovery often turns up an
    // extensioned URL for the same hash even then, so fall back to sniffing
    // those before defaulting to "image" and silently failing every video.
    let is_video =
        ext_is_video || (ext.is_none() && discovered.iter().any(|url| is_video_url(url)));
    if is_video && !ext_is_video {
        if let Some(resp) = serve_cached_video(
            &state,
            &blob_name,
            &dirs,
            &servers,
            &request_headers,
            hit_policy,
        )
        .await?
        {
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

    // Same asymmetry as `/insecure`: video needs multiple round trips, so it
    // gets its own, longer deadline instead of inheriting the image budget.
    let deadline = Instant::now()
        + if is_video {
            state.app.cfg.video_deadline
        } else {
            state.app.cfg.blossom_failover_timeout
        };

    tracing::debug!(
        "Resolved {} server and {} discovered candidates for {}",
        servers.len(),
        discovered.len(),
        blob_name
    );

    // Server hints decide where video and HLS segment bytes come from, so
    // they must split flights: otherwise a request with `xs=attacker` leads
    // the flight and every concurrent viewer of the same hash receives the
    // attacker's thumbnail. Debug formatting keeps the list unambiguous.
    let flight_key = format!("{cache_key}|{servers:?}");

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
        inflight
            .run(&flight_key, move || {
                produce_derivative(state, source, dirs, THUMB_ROUTE, deadline)
            })
            .await?
    };

    // Decided from what this flight actually did: an HLS playlist is only
    // recognised by sniffing after the fetch, and a frame from a discovered
    // origin is served but never persisted — neither may be pinned.
    let persisted_path = outcome.value.cache_path.clone();
    let fresh_policy =
        signed_expiry
            .map(ClientCachePolicy::ExpiresAt)
            .unwrap_or(if persisted_path.is_some() {
                ClientCachePolicy::Immutable
            } else {
                ClientCachePolicy::ShortLived
            });

    let mut resp = fresh_response(
        outcome.value.bytes,
        mime,
        persisted_path.as_deref().unwrap_or(&cache_path),
        outcome.coalesced,
        fresh_policy,
    );
    if persisted_path.is_none() {
        // A stable ETag here would let a client's `If-None-Match`
        // short-circuit to 304 for content that was never pinned server-side
        // and may differ on the next request.
        resp.headers_mut().remove(header::ETAG);
    }
    Ok(resp)
}

/// Serve a cached video thumbnail from the first of the request's own
/// servers that has one (see [`video_identities`]).
async fn serve_cached_video(
    state: &CombinedState,
    blob_name: &str,
    dirs: &Directives,
    servers: &[String],
    request_headers: &HeaderMap,
    policy: ClientCachePolicy,
) -> Result<Option<Response>, SvcError> {
    let cfg = &state.app.cfg;
    for identity in video_identities(blob_name, servers) {
        let key = derivative_cache_key(THUMB_ROUTE, &identity, dirs);
        let path = cache_path_for(cfg, THUMB_ROUTE, &key, &dirs.out_fmt);
        let mime = dirs.out_fmt.mime_type();
        if let Some(resp) = serve_cached(&path, mime, request_headers, policy).await? {
            return Ok(Some(resp));
        }
    }
    Ok(None)
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
    fn method_label_folds_extension_methods_into_one_value() {
        for raw in ["FOO123", "PROPFIND", "PUT"] {
            let method = axum::http::Method::from_bytes(raw.as_bytes()).unwrap();
            assert_eq!(method_label(&method), "OTHER", "{raw}");
        }
        assert_eq!(method_label(&axum::http::Method::GET), "GET");
    }

    #[test]
    fn video_identities_follow_request_server_order_per_origin() {
        let servers = vec![
            "https://evil.example/".to_owned(),
            "https://cdn.example/".to_owned(),
            "https://CDN.example:443/blobs/".to_owned(),
            "not a url".to_owned(),
        ];
        assert_eq!(
            video_identities("h.mp4", &servers),
            vec![
                "h.mp4@https://evil.example".to_owned(),
                "h.mp4@https://cdn.example".to_owned(),
            ]
        );
        // A request that never names evil.example never looks up its entry.
        assert_eq!(
            video_identities("h.mp4", &servers[1..]),
            vec!["h.mp4@https://cdn.example".to_owned()]
        );
    }
}
