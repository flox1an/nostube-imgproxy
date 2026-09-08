use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime},
};

use axum::{
    extract::{Path as AxPath, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
    routing::get,
    Router,
};
use tokio::{
    io::AsyncReadExt,
    sync::{oneshot, Semaphore},
};
use tracing::{error, info, warn};

use crate::{
    blossom::{
        extract_blossom_hash, CandidateFailureCache, CandidateFailureClass, CandidateFailureSummary,
    },
    error::SvcError,
    hls::{budget_counting_body, fetch_playlist, HLS_DEMUXER, HLS_PROTOCOL_WHITELIST},
    metrics,
    network_policy::validate_untrusted_url,
};

const MAX_FFMPEG_STDERR_BYTES: usize = 8 * 1024;
#[cfg(target_os = "linux")]
const FFMPEG_ADDRESS_LIMIT_BYTES: u64 = 1024 * 1024 * 1024;
const FFMPEG_FILE_LIMIT_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone)]
pub struct ThumbnailState {
    pub ffmpeg_semaphore: Arc<Semaphore>,
}

impl ThumbnailState {
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            ffmpeg_semaphore: Arc::new(Semaphore::new(max_concurrent)),
        }
    }
}

/// Return the explicit FFmpeg demuxer for a supported video URL.
///
/// Direct container extensions map to their demuxers. HLS playlists (`.m3u8`)
/// map to the `hls` demuxer and are only safe because the gateway rewrites
/// their segment URIs onto the loopback proxy (see [`crate::hls`]); the same
/// rewriting is what makes a playlist published under a foreign extension
/// (`#EXTM3U` sniffing) usable.
fn input_demuxer(url: &str) -> Option<&'static str> {
    let lower = url.to_ascii_lowercase();
    let path = lower.split('?').next().unwrap_or(&lower);
    if path.ends_with(".mp4")
        || path.ends_with(".mov")
        || path.ends_with(".quicktime")
        || path.ends_with(".qt")
        || path.ends_with(".m4v")
        || path.ends_with(".3gp")
    {
        Some("mov")
    } else if path.ends_with(".webm") || path.ends_with(".mkv") {
        Some("matroska")
    } else if path.ends_with(".avi") {
        Some("avi")
    } else if path.ends_with(".flv") {
        Some("flv")
    } else if path.ends_with(".wmv") {
        Some("asf")
    } else if path.ends_with(".mpg") || path.ends_with(".mpeg") {
        Some("mpeg")
    } else if path.ends_with(".ogv") {
        Some("ogg")
    } else if path.ends_with(".m3u8") || path.ends_with(".m3u") {
        Some(HLS_DEMUXER)
    } else {
        None
    }
}

/// Check if a URL maps to one of the direct-file video formats we can safely
/// pass through the guarded local media proxy.
pub fn is_video_url(url: &str) -> bool {
    input_demuxer(url).is_some()
}

#[derive(Clone)]
struct MediaProxyState {
    source_url: String,
    token: String,
    http: reqwest::Client,
    deadline: Instant,
    remaining_bytes: Arc<AtomicU64>,
}

/// A short-lived, loopback-only HTTP gateway for one remote video URL.
///
/// FFmpeg supports seeking by sending HTTP Range requests. Giving it this
/// gateway preserves that behaviour for multi-gigabyte files while every range
/// fetch still uses our public-DNS resolver and redirect policy. It is not a
/// general proxy: the random path token is bound to one source URL and a shared
/// byte budget, and the listener lives only for one FFmpeg invocation.
struct LocalMediaProxy {
    input_url: String,
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl LocalMediaProxy {
    async fn start(
        source_url: String,
        http: reqwest::Client,
        deadline: Instant,
        max_probe_bytes: u64,
    ) -> Result<Self, SvcError> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(SvcError::Io)?;
        let address = listener.local_addr().map_err(SvcError::Io)?;
        let token = proxy_token();
        let state = MediaProxyState {
            source_url,
            token: token.clone(),
            http,
            deadline,
            remaining_bytes: Arc::new(AtomicU64::new(max_probe_bytes)),
        };
        let app = Router::new()
            .route("/{token}", get(serve_media_range))
            .with_state(state);
        let (shutdown, receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = receiver.await;
                })
                .await;
        });

        Ok(Self {
            input_url: format!("http://127.0.0.1:{}/{token}", address.port()),
            shutdown: Some(shutdown),
            task,
        })
    }
}

impl Drop for LocalMediaProxy {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.task.abort();
    }
}

async fn serve_media_range(
    State(state): State<MediaProxyState>,
    AxPath(token): AxPath<String>,
    request_headers: HeaderMap,
) -> Result<Response, StatusCode> {
    if token != state.token {
        return Err(StatusCode::NOT_FOUND);
    }

    // A whole-resource 200 response would make an adversarial origin stream
    // forever. FFmpeg uses Range for seekable containers; rejecting the other
    // case is safer than silently degrading to a full download.
    let range = request_headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.starts_with("bytes="))
        .ok_or(StatusCode::RANGE_NOT_SATISFIABLE)?;
    let remaining = state.deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(StatusCode::GATEWAY_TIMEOUT);
    }

    // FFmpeg's mov/mp4 demuxer opens with an unbounded `Range: bytes=0-`
    // probe (and reopens the same way after every seek) — it relies on
    // reading only as much of the stream as it needs before seeking again,
    // not on the origin's declared length. Forwarding that request verbatim
    // makes a multi-gigabyte origin answer with a multi-gigabyte
    // Content-Length, which used to fail the whole candidate before a single
    // byte was read. Bounding the outbound end to the remaining budget keeps
    // the origin's declared length honest without limiting how large a video
    // FFmpeg can seek around in.
    let budget = state.remaining_bytes.load(Ordering::Acquire);
    if budget == 0 {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }
    let outbound_range = clamp_open_range(range, budget);

    let upstream = tokio::time::timeout(
        remaining,
        state
            .http
            .get(&state.source_url)
            .header(reqwest::header::RANGE, outbound_range)
            .send(),
    )
    .await
    .map_err(|_| StatusCode::GATEWAY_TIMEOUT)?
    .map_err(|_| StatusCode::BAD_GATEWAY)?;

    if upstream.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(if upstream.status().is_success() {
            StatusCode::RANGE_NOT_SATISFIABLE
        } else {
            StatusCode::BAD_GATEWAY
        });
    }

    // Safety net for an origin that ignores our bounded end and answers with
    // more than it was asked for; the per-chunk counter below is the actual
    // enforcement for a compliant origin.
    let announced = upstream.content_length().unwrap_or(0);
    if announced > budget {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }

    // Copy only Range semantics and the media type. Redirect, cookie, CORS and
    // caching headers belong to the remote origin and must never reach FFmpeg.
    let response_headers: Vec<_> = [
        header::CONTENT_LENGTH,
        header::CONTENT_RANGE,
        header::ACCEPT_RANGES,
        header::CONTENT_TYPE,
    ]
    .into_iter()
    .filter_map(|name| {
        upstream
            .headers()
            .get(&name)
            .cloned()
            .map(|value| (name, value))
    })
    .collect();
    let mut response = Response::new(budget_counting_body(
        upstream.bytes_stream(),
        Arc::clone(&state.remaining_bytes),
    ));
    *response.status_mut() = StatusCode::PARTIAL_CONTENT;

    for (name, value) in response_headers {
        response.headers_mut().insert(name, value);
    }
    Ok(response)
}

/// Bound an incoming, open-ended byte-range spec (`bytes=N-`) to `budget`
/// bytes so the outbound request to the origin can never be answered with
/// more than the remaining probe allowance. Explicit-end ranges (`bytes=N-M`)
/// and suffix ranges (`bytes=-N`) are passed through unchanged: they already
/// bound the origin's response themselves.
fn clamp_open_range(range: &str, budget: u64) -> String {
    if let Some((start, end)) = range
        .strip_prefix("bytes=")
        .and_then(|spec| spec.split_once('-'))
    {
        if end.is_empty() {
            if let Ok(start) = start.parse::<u64>() {
                return format!("bytes={start}-{}", start.saturating_add(budget - 1));
            }
        }
    }
    range.to_string()
}

pub(crate) fn proxy_token() -> String {
    use std::hash::{BuildHasher, Hasher};

    static RANDOM: std::sync::LazyLock<std::collections::hash_map::RandomState> =
        std::sync::LazyLock::new(std::collections::hash_map::RandomState::new);
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut hasher = RANDOM.build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    hasher.write_u128(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos()),
    );
    format!("{:016x}", hasher.finish())
}

/// Extract a video thumbnail by trying server-derived and discovered candidates.
///
/// Every FFmpeg input is a loopback URL from [`LocalMediaProxy`]. It therefore
/// retains HTTP range seeking without allowing FFmpeg to resolve or contact a
/// remote host itself. `max_probe_bytes` caps total source bytes across all
/// Range responses for a candidate; it is deliberately not a full video-size
/// cap, so large seekable videos remain supported.
#[allow(clippy::too_many_arguments)]
pub async fn extract_video_thumbnail(
    video_url: &str,
    semaphore: &Arc<Semaphore>,
    http: &reqwest::Client,
    servers: &[String],
    discovered_urls: &[String],
    negative_cache: Option<&CandidateFailureCache>,
    expected_hash: Option<&str>,
    max_candidates: usize,
    max_probe_bytes: u64,
    max_image_bytes: usize,
    deadline: Instant,
    ffmpeg_timeout: Duration,
) -> Result<Vec<u8>, SvcError> {
    info!(source = %log_value(video_url), "extracting video thumbnail");

    let _permit = semaphore
        .acquire()
        .await
        .map_err(|_| SvcError::InternalError("ffmpeg semaphore closed".into()))?;

    let mut candidates: Vec<String> = vec![video_url.to_string()];
    if !servers.is_empty() {
        if let Some((hash, ext)) = extract_blossom_hash(video_url) {
            candidates.extend(
                servers
                    .iter()
                    .map(|server| format!("{}/{}.{}", server.trim_end_matches('/'), hash, ext)),
            );
        }
    }
    candidates.extend(discovered_urls.iter().cloned());

    let mut seen = HashSet::new();
    candidates.retain(|url| {
        input_demuxer(url).is_some()
            && validate_untrusted_url(url).is_ok()
            && seen.insert(url.clone())
    });
    candidates.truncate(max_candidates.max(1));
    if candidates.is_empty() {
        return Err(SvcError::BadRequest("no safe video thumbnail source"));
    }

    let mut failures = CandidateFailureSummary::default();
    let mut last_error = SvcError::UpstreamError(404);
    let mut attempted = 0;

    for (idx, url) in candidates.iter().enumerate() {
        if let (Some(cache), Some(hash)) = (negative_cache, expected_hash) {
            if let Some(class) = cache.lookup(hash, url).await {
                metrics::record_cache_hit("blossom_negative");
                failures.record(class);
                continue;
            }
        }
        attempted += 1;

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            last_error = SvcError::UpstreamError(504);
            remember_failure(
                negative_cache,
                expected_hash,
                url,
                CandidateFailureClass::Transient,
            )
            .await;
            failures.record(CandidateFailureClass::Transient);
            break;
        }

        let resolved = match preflight_candidate(http, url, deadline).await {
            Ok(resolved) => resolved,
            Err(error) => {
                let class = CandidateFailureClass::from_error(&error);
                remember_failure(negative_cache, expected_hash, url, class).await;
                failures.record(class);
                last_error = error;
                continue;
            }
        };

        let attempt_budget = remaining.min(ffmpeg_timeout);
        let demuxer = input_demuxer(url).expect("candidate filter retains supported videos");
        match extract_thumbnail_with_ffmpeg(
            resolved,
            demuxer,
            http.clone(),
            deadline,
            max_probe_bytes,
            max_image_bytes,
            attempt_budget,
        )
        .await
        {
            Ok(bytes) => {
                metrics::record_bytes_downloaded("video", bytes.len());
                info!(
                    candidate = idx + 1,
                    bytes = bytes.len(),
                    "video thumbnail extracted"
                );
                return Ok(bytes);
            }
            Err(error) => {
                let class = CandidateFailureClass::from_error(&error);
                remember_failure(negative_cache, expected_hash, url, class).await;
                failures.record(class);
                last_error = error;
            }
        }
    }

    if attempted == 0 && negative_cache.is_some() && expected_hash.is_some() {
        return Err(failures.into_error());
    }
    // Every candidate was tried and none produced a thumbnail. This is the
    // one place that can tell an operator *why* a specific video never gets
    // a thumbnail — per-candidate failures above are recorded silently into
    // the negative-candidate cache, and callers only see a generic HTTP
    // status. Warn (not debug) so it shows under the default `RUST_LOG=info`.
    warn!(
        source = %log_value(video_url),
        candidates = candidates.len(),
        attempted,
        error = ?last_error,
        "video thumbnail extraction exhausted every candidate"
    );
    Err(last_error)
}

async fn remember_failure(
    cache: Option<&CandidateFailureCache>,
    expected_hash: Option<&str>,
    url: &str,
    class: CandidateFailureClass,
) {
    if let (Some(cache), Some(hash)) = (cache, expected_hash) {
        cache.remember(hash, url, class).await;
    }
}

/// Resolve a candidate through the guarded Reqwest client before starting the
/// local proxy. Every actual FFmpeg range request is guarded again by that same
/// client, so this preflight is only an inexpensive early failure path.
async fn preflight_candidate(
    http: &reqwest::Client,
    url: &str,
    deadline: Instant,
) -> Result<String, SvcError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(SvcError::UpstreamError(504));
    }
    let response = tokio::time::timeout(
        remaining,
        http.get(url)
            .header(reqwest::header::RANGE, "bytes=0-0")
            .send(),
    )
    .await
    .map_err(|_| SvcError::UpstreamError(504))?
    .map_err(SvcError::Fetch)?;

    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(if response.status().is_success() {
            SvcError::UpstreamError(416)
        } else {
            SvcError::UpstreamError(response.status().as_u16())
        });
    }
    Ok(response.url().as_str().to_owned())
}

/// Spawn one constrained FFmpeg process reading only from `input`. Output is
/// the thumbnail directly; there is no intermediate clip and no full
/// source-video download to disk beyond `input` itself.
async fn run_ffmpeg_extract(
    input: &str,
    protocol_whitelist: &str,
    demuxer: &str,
    max_image_bytes: usize,
    timeout: Duration,
) -> Result<Vec<u8>, SvcError> {
    use tokio::process::Command;

    let frames_dir = tempfile::tempdir().map_err(SvcError::Io)?;
    let output_pattern = frames_dir.path().join("f%03d.webp");

    let mut command = Command::new("ffmpeg");
    // Input seek (`-ss` before `-i`) is the cheap path for direct containers:
    // FFmpeg seeks by byte offset without decoding. FFmpeg's HLS demuxer,
    // however, silently produces zero frames and a zero exit for an input
    // seek, so playlists take the output-seek order instead: decode-and-
    // discard from the head, then take the first frame at/after 0.5s.
    let seek_and_input: &[&str] = if demuxer == HLS_DEMUXER {
        &["-f", demuxer, "-i", input, "-ss", "0.5"]
    } else {
        &["-ss", "0.5", "-f", demuxer, "-i", input]
    };
    command
        .args([
            "-nostdin",
            "-loglevel",
            "error",
            // Explicit input demuxing below blocks playlist parsing from
            // introducing nested remote URLs under this otherwise narrow
            // whitelist. For the loopback-proxy caller, `tcp` must be listed
            // explicitly: FFmpeg opens HTTP's transport through the `tcp`
            // protocol, and without it every range fetch fails with
            // "Protocol 'tcp' not on whitelist". `https`/`tls` stay excluded
            // so the loopback proxy remains the only network path; the
            // verified-local-file caller passes `file` only, so FFmpeg has no
            // network access at all.
            "-protocol_whitelist",
            protocol_whitelist,
            "-analyzeduration",
            "5000000",
            "-probesize",
            "5000000",
            // Frame-parallel decode of certain VP9 streams triggers a known
            // libavcodec/libvpx assertion failure ("A decoder returned an
            // unexpected error code. This is a bug, please report it."),
            // deterministically crashing the decode for that file every time
            // rather than intermittently. Pinning decode to one thread avoids
            // the frame-threaded code path entirely.
            "-threads",
            "1",
        ])
        .args(seek_and_input)
        .args([
            // Sample ~1 frame/second across the first 8 seconds and let the
            // scorer pick the first contentful one: single-frame extraction
            // at 0.5s turns black lead-ins (fade-ins, muxer padding) into
            // pitch-black thumbnails. The luma thresholds and the 8-frame
            // ceiling live in `transform::first_contentful_frame`'s docs.
            "-t",
            "8",
            "-map",
            "0:v:0",
            "-frames:v",
            "8",
            "-vf",
            "fps=1,scale='min(1280,iw)':'min(720,ih)':force_original_aspect_ratio=decrease",
            "-q:v",
            "80",
            "-c:v",
            "libwebp",
            "-f",
            "image2",
            "-y",
        ])
        .arg(&output_pattern)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    apply_ffmpeg_limits(&mut command, timeout)?;

    let mut child = command.spawn().map_err(|error| {
        error!(error = %error, "failed to spawn ffmpeg");
        SvcError::InternalError("failed to spawn ffmpeg".into())
    })?;
    let stderr = child.stderr.take().expect("stderr is piped");
    let stderr_task = tokio::spawn(read_capped(stderr, MAX_FFMPEG_STDERR_BYTES));

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(result) => result.map_err(SvcError::Io)?,
        Err(_) => {
            metrics::record_ffmpeg_extraction(false);
            warn!(
                demuxer,
                timeout_secs = timeout.as_secs(),
                "ffmpeg did not finish within its timeout budget"
            );
            return Err(SvcError::UpstreamError(504));
        }
    };
    let stderr = stderr_task.await.unwrap_or_else(|_| Vec::new());
    if !status.success() {
        metrics::record_ffmpeg_extraction(false);
        let stderr = String::from_utf8_lossy(&stderr);
        tracing::warn!(stderr = %log_value(&stderr), "ffmpeg thumbnail extraction failed");
        // Audio-only HLS inputs exit 0 having matched no video stream at all;
        // that is a property of the source, not an upstream fault.
        if stderr.contains("matches no streams") {
            return Err(SvcError::BadRequest("source has no video stream"));
        }
        return Err(SvcError::UpstreamError(502));
    }

    let mut frame_paths: Vec<std::path::PathBuf> = Vec::new();
    let mut entries = tokio::fs::read_dir(frames_dir.path())
        .await
        .map_err(SvcError::Io)?;
    while let Some(entry) = entries.next_entry().await.map_err(SvcError::Io)? {
        if entry.path().extension().is_some_and(|ext| ext == "webp") {
            frame_paths.push(entry.path());
        }
    }
    frame_paths.sort();
    if frame_paths.is_empty() {
        metrics::record_ffmpeg_extraction(false);
        tracing::debug!(demuxer, "ffmpeg produced no usable image");
        return Err(SvcError::UpstreamError(502));
    }

    let mut frame_bytes: Vec<Vec<u8>> = Vec::with_capacity(frame_paths.len());
    for path in frame_paths {
        let metadata = tokio::fs::metadata(&path).await.map_err(SvcError::Io)?;
        if metadata.len() == 0 || metadata.len() > max_image_bytes as u64 {
            continue;
        }
        frame_bytes.push(tokio::fs::read(&path).await.map_err(SvcError::Io)?);
    }
    if frame_bytes.is_empty() {
        metrics::record_ffmpeg_extraction(false);
        tracing::debug!(max_image_bytes, demuxer, "ffmpeg produced no usable image");
        return Err(SvcError::UpstreamError(502));
    }

    // WebP decode of up to 8 frames is real CPU work; keep it off the async
    // workers like every other decode in the service.
    let (chosen, frame_bytes) = tokio::task::spawn_blocking(move || {
        let chosen = crate::transform::first_contentful_frame(&frame_bytes).unwrap_or(0);
        (chosen, frame_bytes)
    })
    .await
    .map_err(|_| SvcError::InternalError("frame scorer crashed".into()))?;
    tracing::debug!(chosen, "selected thumbnail frame");
    metrics::record_ffmpeg_extraction(true);
    Ok(frame_bytes
        .into_iter()
        .nth(chosen)
        .expect("index from scorer"))
}

#[allow(clippy::too_many_arguments)]
async fn extract_thumbnail_with_ffmpeg(
    source_url: String,
    demuxer: &str,
    http: reqwest::Client,
    deadline: Instant,
    max_probe_bytes: u64,
    max_image_bytes: usize,
    timeout: Duration,
) -> Result<Vec<u8>, SvcError> {
    // A playlist input needs the HLS gateway: the playlist is fetched and
    // rewritten once, and FFmpeg's segment requests loop back through it.
    if demuxer == HLS_DEMUXER {
        let budget = Arc::new(AtomicU64::new(max_probe_bytes));
        let playlist = fetch_playlist(&http, &source_url, deadline, &budget).await?;
        let proxy =
            crate::hls::HlsMediaProxy::start(playlist, &source_url, http, deadline, budget).await?;
        run_ffmpeg_extract(
            &proxy.input_url,
            HLS_PROTOCOL_WHITELIST,
            demuxer,
            max_image_bytes,
            timeout,
        )
        .await
    } else {
        let proxy = LocalMediaProxy::start(source_url, http, deadline, max_probe_bytes).await?;
        run_ffmpeg_extract(
            &proxy.input_url,
            "file,http,tcp",
            demuxer,
            max_image_bytes,
            timeout,
        )
        .await
    }
}

/// Extract a thumbnail frame from a video blob whose bytes are already
/// hash-verified and fully local (e.g. via
/// [`crate::blossom::try_fetch_verified_blob`]). FFmpeg gets a
/// `file`-only protocol whitelist — no network access at all — since the
/// whole input already sits on disk; there is no candidate loop or byte
/// budget to enforce here, both already happened before this bytes were
/// obtained.
pub async fn extract_thumbnail_from_verified_bytes(
    bytes: &[u8],
    blob_name: &str,
    semaphore: &Arc<Semaphore>,
    max_image_bytes: usize,
    timeout: Duration,
) -> Result<Vec<u8>, SvcError> {
    let demuxer =
        input_demuxer(blob_name).ok_or(SvcError::BadRequest("unsupported video format"))?;

    let _permit = semaphore
        .acquire()
        .await
        .map_err(|_| SvcError::InternalError("ffmpeg semaphore closed".into()))?;

    let input_file = tempfile::NamedTempFile::new().map_err(SvcError::Io)?;
    tokio::fs::write(input_file.path(), bytes)
        .await
        .map_err(SvcError::Io)?;
    let input_path = input_file.path().to_string_lossy().into_owned();

    run_ffmpeg_extract(&input_path, "file", demuxer, max_image_bytes, timeout).await
}

/// Extract a thumbnail from an HLS playlist whose bytes are already
/// hash-verified and fully local (e.g. via
/// [`crate::blossom::try_fetch_verified_blob`] or the image-path
/// `#EXTM3U` sniff). The playlist is served from memory through the
/// [`crate::hls::HlsMediaProxy`]; only its *segments* are still remote, so
/// unlike [`extract_thumbnail_from_verified_bytes`] this needs network
/// access (`http` in the whitelist, loopback gateway only) and a probe
/// budget. The resulting thumbnail is derived from unverified segment bytes
/// and must be treated as such by callers (never hash-keyed cached).
#[allow(clippy::too_many_arguments)]
pub async fn extract_thumbnail_from_verified_playlist(
    playlist: &[u8],
    base_url: &str,
    semaphore: &Arc<Semaphore>,
    http: &reqwest::Client,
    max_probe_bytes: u64,
    max_image_bytes: usize,
    deadline: Instant,
    timeout: Duration,
) -> Result<Vec<u8>, SvcError> {
    let _permit = semaphore
        .acquire()
        .await
        .map_err(|_| SvcError::InternalError("ffmpeg semaphore closed".into()))?;

    let budget = Arc::new(AtomicU64::new(max_probe_bytes));
    let proxy = crate::hls::HlsMediaProxy::start(
        playlist.to_vec(),
        base_url,
        http.clone(),
        deadline,
        budget,
    )
    .await?;
    run_ffmpeg_extract(
        &proxy.input_url,
        HLS_PROTOCOL_WHITELIST,
        HLS_DEMUXER,
        max_image_bytes,
        timeout,
    )
    .await
}

async fn read_capped<R>(mut reader: R, cap: usize) -> Vec<u8>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut output = Vec::with_capacity(cap);
    let mut buffer = [0u8; 1024];
    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let keep = cap.saturating_sub(output.len()).min(read);
        output.extend_from_slice(&buffer[..keep]);
    }
    output
}

#[cfg(unix)]
fn apply_ffmpeg_limits(
    command: &mut tokio::process::Command,
    timeout: Duration,
) -> Result<(), SvcError> {
    use std::os::unix::process::CommandExt;

    let cpu_seconds = timeout.as_secs().saturating_add(1).max(1);
    unsafe {
        command.as_std_mut().pre_exec(move || {
            // Darwin has no address-space rlimit: `setrlimit(RLIMIT_AS, …)`
            // fails with EINVAL, which aborts the whole spawn. Linux enforces
            // it (production); macOS ignores the concept entirely.
            #[cfg(target_os = "linux")]
            let limits = [
                (libc::RLIMIT_AS, FFMPEG_ADDRESS_LIMIT_BYTES),
                (libc::RLIMIT_CPU, cpu_seconds),
                (libc::RLIMIT_FSIZE, FFMPEG_FILE_LIMIT_BYTES),
            ];
            #[cfg(not(target_os = "linux"))]
            let limits = [
                (libc::RLIMIT_CPU, cpu_seconds),
                (libc::RLIMIT_FSIZE, FFMPEG_FILE_LIMIT_BYTES),
            ];
            for (resource, limit) in limits {
                let value = libc::rlimit {
                    rlim_cur: limit as libc::rlim_t,
                    rlim_max: limit as libc::rlim_t,
                };
                if libc::setrlimit(resource, &value) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn apply_ffmpeg_limits(_: &mut tokio::process::Command, _: Duration) -> Result<(), SvcError> {
    Ok(())
}

fn log_value(value: &str) -> &str {
    value.get(..value.len().min(128)).unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_video_url_accepts_containers_and_m3u8_but_rejects_other_manifests() {
        assert!(is_video_url("https://cdn.example/video.mp4"));
        assert!(is_video_url("https://cdn.example/video.webm?download=1"));
        assert!(is_video_url("https://cdn.example/video.quicktime"));
        assert!(is_video_url("https://cdn.example/video.qt"));
        // HLS playlists route through the rewriting gateway, DASH still has
        // no entry.
        assert!(is_video_url("https://cdn.example/video.m3u8"));
        assert!(!is_video_url("https://cdn.example/video.mpd"));
    }

    #[tokio::test]
    async fn media_proxy_rejects_non_range_requests() {
        crate::init_crypto_provider();
        let state = MediaProxyState {
            source_url: "https://cdn.example/video.mp4".into(),
            token: "correct".into(),
            http: reqwest::Client::new(),
            deadline: Instant::now() + Duration::from_secs(1),
            remaining_bytes: Arc::new(AtomicU64::new(1024)),
        };
        let error = serve_media_range(State(state), AxPath("correct".into()), HeaderMap::new())
            .await
            .expect_err("whole-file proxy requests must be rejected");
        assert_eq!(error, StatusCode::RANGE_NOT_SATISFIABLE);
    }

    #[tokio::test]
    async fn thumbnail_skips_hash_scoped_negative_candidate() {
        crate::init_crypto_provider();
        let cache = CandidateFailureCache::new(
            Duration::from_secs(60),
            Duration::from_secs(60),
            Duration::from_secs(60),
        );
        let hash = "a".repeat(64);
        let url = format!("https://cdn.example/{hash}.mp4");
        cache
            .remember(&hash, &url, CandidateFailureClass::Missing)
            .await;
        let result = extract_video_thumbnail(
            &url,
            &Arc::new(Semaphore::new(1)),
            &reqwest::Client::new(),
            &[],
            &[],
            Some(&cache),
            Some(&hash),
            8,
            1024,
            1024,
            Instant::now() + Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .await;
        assert!(matches!(result, Err(SvcError::UpstreamError(404))));
    }
    #[tokio::test]
    async fn media_proxy_relays_ranges_without_downloading_the_full_video() {
        crate::init_crypto_provider();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let app = Router::new().route(
                "/video.mp4",
                get(|| async {
                    (
                        StatusCode::PARTIAL_CONTENT,
                        [
                            (header::CONTENT_RANGE, "bytes 0-3/1073741824"),
                            (header::ACCEPT_RANGES, "bytes"),
                        ],
                        "clip",
                    )
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });

        let http = reqwest::Client::builder()
            .resolve("cdn.example", address)
            .build()
            .unwrap();
        let proxy = LocalMediaProxy::start(
            format!("http://cdn.example:{}/video.mp4", address.port()),
            http,
            Instant::now() + Duration::from_secs(1),
            4,
        )
        .await
        .unwrap();
        let response = reqwest::Client::new()
            .get(&proxy.input_url)
            .header(reqwest::header::RANGE, "bytes=0-3")
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.bytes().await.unwrap().as_ref(), b"clip");
    }

    #[test]
    fn clamp_open_range_bounds_an_open_ended_spec_to_the_budget() {
        assert_eq!(clamp_open_range("bytes=0-", 64), "bytes=0-63");
        assert_eq!(clamp_open_range("bytes=1000-", 64), "bytes=1000-1063");
    }

    #[test]
    fn clamp_open_range_leaves_bounded_and_suffix_specs_untouched() {
        assert_eq!(clamp_open_range("bytes=0-3", 64), "bytes=0-3");
        assert_eq!(clamp_open_range("bytes=-500", 64), "bytes=-500");
    }

    #[tokio::test]
    async fn media_proxy_clamps_an_open_ended_probe_against_a_huge_origin() {
        crate::init_crypto_provider();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let app = Router::new().route(
                "/video.mp4",
                get(|request_headers: HeaderMap| async move {
                    // A real origin answers an open-ended `bytes=0-` request
                    // with the entire remaining file. Simulating a
                    // multi-gigabyte source proves the proxy never forwards
                    // that request unbounded: an unclamped forward would make
                    // this handler report a multi-gigabyte Content-Length and
                    // the caller would see 413 instead of 206.
                    let range = request_headers
                        .get(header::RANGE)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    let (start, end) = range
                        .strip_prefix("bytes=")
                        .and_then(|spec| spec.split_once('-'))
                        .unwrap_or_default();
                    assert!(!end.is_empty(), "expected a bounded range, got {range:?}");
                    let start: u64 = start.parse().unwrap();
                    let end: u64 = end.parse().unwrap();
                    let body = vec![b'x'; (end - start + 1) as usize];
                    (
                        StatusCode::PARTIAL_CONTENT,
                        [
                            (
                                header::CONTENT_RANGE,
                                format!("bytes {start}-{end}/2000000000"),
                            ),
                            (header::ACCEPT_RANGES, "bytes".to_string()),
                        ],
                        body,
                    )
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });

        let http = reqwest::Client::builder()
            .resolve("cdn.example", address)
            .build()
            .unwrap();
        let proxy = LocalMediaProxy::start(
            format!("http://cdn.example:{}/video.mp4", address.port()),
            http,
            Instant::now() + Duration::from_secs(1),
            16,
        )
        .await
        .unwrap();
        let response = reqwest::Client::new()
            .get(&proxy.input_url)
            .header(reqwest::header::RANGE, "bytes=0-")
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.bytes().await.unwrap().len(), 16);
    }

    /// A 1s black lead-in followed by test content: the thumbnail must come
    /// from the content, not the lead-in. Skips itself when no `ffmpeg`
    /// binary is installed.
    #[tokio::test]
    async fn extraction_picks_non_black_frame_after_black_lead_in() {
        if std::process::Command::new("ffmpeg")
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_or(true, |status| !status.success())
        {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        crate::init_crypto_provider();

        let dir = tempfile::tempdir().expect("tempdir");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=black:s=128x96:r=10:d=1",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=128x96:rate=10:duration=1",
                "-filter_complex",
                "[0:v][1:v]concat=n=2:v=1:a=0,format=yuv420p",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
            ])
            .arg(dir.path().join("lead.mp4"))
            .status()
            .expect("spawn ffmpeg");
        assert!(status.success(), "fixture generation failed");
        let bytes = tokio::fs::read(dir.path().join("lead.mp4"))
            .await
            .expect("read fixture");

        let semaphore = Arc::new(Semaphore::new(1));
        let thumbnail = extract_thumbnail_from_verified_bytes(
            &bytes,
            "lead.mp4",
            &semaphore,
            1024 * 1024,
            Duration::from_secs(30),
        )
        .await
        .expect("extraction succeeds");

        // The scorer itself must classify the result as content — the old
        // single-frame-at-0.5s behaviour produced a frame it would reject.
        assert!(
            crate::transform::first_contentful_frame(std::slice::from_ref(&thumbnail)).is_some(),
            "thumbnail must not be a black lead-in frame"
        );
    }
}
