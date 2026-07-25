use std::{sync::Arc, time::Instant};
use tokio::sync::Semaphore;
use tracing::{error, info};

use crate::{
    blossom::{
        extract_blossom_hash, CandidateFailureCache, CandidateFailureClass, CandidateFailureSummary,
    },
    error::SvcError,
    metrics,
    network_policy::validate_untrusted_url,
};

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

/// Check if a URL is likely a video based on file extension
///
/// Returns true only for known video extensions.
/// All other URLs (including .jfif, .jpg, .jpeg, .png, .webp, .avif, and URLs without extensions)
/// are treated as images and processed with content-based format detection.
pub fn is_video_url(url: &str) -> bool {
    // Strip query string before checking extension
    let url_lower = url.to_lowercase();
    let path = url_lower.split('?').next().unwrap_or(&url_lower);
    path.ends_with(".mp4")
        || path.ends_with(".mov")
        || path.ends_with(".avi")
        || path.ends_with(".webm")
        || path.ends_with(".mkv")
        || path.ends_with(".flv")
        || path.ends_with(".wmv")
        || path.ends_with(".m4v")
        || path.ends_with(".mpg")
        || path.ends_with(".mpeg")
        || path.ends_with(".3gp")
        || path.ends_with(".ogv")
        || path.ends_with(".m3u8")
}

/// Extract a video thumbnail by trying server-derived and discovered candidates.
///
/// `http` is the SSRF-guarded client every candidate is preflighted with, and
/// `ffmpeg_timeout` caps a single FFmpeg run so one stalled upstream cannot
/// hold a semaphore permit indefinitely.
#[allow(clippy::too_many_arguments)]
pub async fn extract_video_thumbnail(
    video_url: &str,
    semaphore: &Arc<Semaphore>,
    http: &reqwest::Client,
    servers: &[String],
    discovered_urls: &[String],
    negative_cache: Option<&CandidateFailureCache>,
    deadline: Instant,
    ffmpeg_timeout: std::time::Duration,
) -> Result<Vec<u8>, SvcError> {
    info!("extracting thumbnail from video: {}", video_url);

    let _permit = semaphore
        .acquire()
        .await
        .map_err(|_| SvcError::Io(std::io::Error::other("semaphore closed")))?;

    // Always try the original URL first, then any server-derived candidates.
    let mut candidates: Vec<String> = vec![video_url.to_string()];

    if !servers.is_empty() {
        if let Some((hash, ext)) = extract_blossom_hash(video_url) {
            for server in servers {
                let url = format!("{}/{}.{}", server.trim_end_matches('/'), hash, ext);
                if url != video_url {
                    candidates.push(url);
                }
            }
        }
    }
    candidates.extend(discovered_urls.iter().cloned());

    let mut seen = std::collections::HashSet::new();
    candidates.retain(|url| validate_untrusted_url(url).is_ok() && seen.insert(url.clone()));

    if candidates.is_empty() {
        return Err(SvcError::BadRequest("no safe video thumbnail source"));
    }

    let mut failures = CandidateFailureSummary::default();
    let mut last_error = SvcError::UpstreamError(404);
    let mut attempted = 0;

    for (idx, url) in candidates.iter().enumerate() {
        if let Some(cache) = negative_cache {
            if let Some(class) = cache.lookup(url).await {
                metrics::record_cache_hit("blossom_negative");
                tracing::debug!(
                    ?class,
                    candidate = %url,
                    "skipping negatively cached video candidate"
                );
                failures.record(class);
                continue;
            }
        }
        attempted += 1;

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            last_error = SvcError::UpstreamError(504);
            if let Some(cache) = negative_cache {
                cache.remember(url, CandidateFailureClass::Transient).await;
                failures.record(CandidateFailureClass::Transient);
            }
            break;
        }
        tracing::debug!(
            "thumbnail attempt {}/{}: {}",
            idx + 1,
            candidates.len(),
            url
        );

        // Cheap guarded probe first: dead or redirecting candidates are
        // rejected in milliseconds instead of costing an FFmpeg spawn.
        if let Err(error) = preflight_candidate(http, url, deadline).await {
            let class = CandidateFailureClass::from_error(&error);
            if let Some(cache) = negative_cache {
                cache.remember(url, class).await;
            }
            failures.record(class);
            tracing::debug!(?class, candidate = %url, "preflight rejected video candidate");
            last_error = error;
            continue;
        }

        let attempt_budget = remaining.min(ffmpeg_timeout);
        match extract_thumbnail_with_ffmpeg(url, attempt_budget).await {
            Ok(bytes) => {
                tracing::info!(
                    "✓ thumbnail {}/{} ({} bytes) from {}",
                    idx + 1,
                    candidates.len(),
                    bytes.len(),
                    url
                );
                return Ok(bytes);
            }
            Err(error) => {
                tracing::debug!(
                    "✗ thumbnail {}/{} failed: {:?}",
                    idx + 1,
                    candidates.len(),
                    error
                );
                let class = CandidateFailureClass::from_error(&error);
                if let Some(cache) = negative_cache {
                    cache.remember(url, class).await;
                }
                failures.record(class);
                last_error = error;
            }
        }
    }

    tracing::warn!(
        "all {} candidates exhausted for {}",
        candidates.len(),
        video_url
    );
    if negative_cache.is_some() && attempted == 0 {
        Err(failures.into_error())
    } else {
        Err(last_error)
    }
}

/// Preflight a candidate with our own HTTP client before handing it to FFmpeg.
///
/// FFmpeg resolves and follows redirects itself, so it bypasses the public-IP
/// DNS resolver and `redirect::Policy::none()` that protect every other fetch
/// in this service. Probing first with the guarded client means a candidate
/// that redirects (or is simply dead) never reaches FFmpeg at all.
async fn preflight_candidate(
    http: &reqwest::Client,
    url: &str,
    deadline: Instant,
) -> Result<(), SvcError> {
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

    let status = response.status();
    if status.is_redirection() {
        // `redirect::Policy::none()` surfaces the 3xx instead of following it.
        // Refuse rather than let FFmpeg chase it to an unvalidated host.
        tracing::debug!(%url, %status, "refusing redirecting video candidate");
        return Err(SvcError::UpstreamError(502));
    }
    if !status.is_success() {
        return Err(SvcError::UpstreamError(status.as_u16()));
    }
    Ok(())
}

/// Extract a thumbnail from a video using the FFmpeg CLI.
///
/// `timeout` is a hard wall-clock bound. Without one, `Command::output()` waits
/// forever on a stalled upstream and the semaphore permit is never returned, so
/// a handful of hung fetches permanently disable video thumbnails.
async fn extract_thumbnail_with_ffmpeg(
    video_url: &str,
    timeout: std::time::Duration,
) -> Result<Vec<u8>, SvcError> {
    use tokio::process::Command;

    let temp_file = tempfile::NamedTempFile::new().map_err(SvcError::Io)?;
    let output_path = temp_file.path();

    tracing::debug!("spawning ffmpeg for video: {}", video_url);

    // `-rw_timeout` is in microseconds and bounds each socket read/write, so a
    // trickling upstream cannot outlive the wall-clock budget by stalling.
    let io_timeout_micros = (timeout.as_micros().min(u128::from(u32::MAX)) as u64).to_string();

    let child = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-loglevel",
            "error",
            // Confine FFmpeg to network protocols: without this it will happily
            // open `file:`, `concat:` or `subfile:` targets from a playlist.
            "-protocol_whitelist",
            "http,https,tcp,tls,crypto",
            "-user_agent",
            "rust-imgproxy/0.1",
            "-rw_timeout",
            &io_timeout_micros,
            "-analyzeduration",
            "5000000",
            "-probesize",
            "5000000",
            "-ss",
            "0.5",
            "-i",
            video_url,
            "-vframes",
            "1",
            "-vf",
            "scale=-1:'min(720,ih)'",
            "-q:v",
            "80",
            "-c:v",
            "libwebp",
            "-f",
            "image2",
            "-y",
        ])
        .arg(output_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Dropping the future on timeout must actually reap the process rather
        // than orphan it holding an upstream connection open.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            error!("failed to spawn ffmpeg for {}: {}", video_url, e);
            SvcError::Io(e)
        })?;

    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(result) => result.map_err(SvcError::Io)?,
        Err(_) => {
            tracing::debug!("ffmpeg exceeded {:?} for {}", timeout, video_url);
            metrics::record_ffmpeg_extraction(false);
            return Err(SvcError::UpstreamError(504));
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Check for common error patterns
        let is_timeout = stderr.contains("timed out") || stderr.contains("Connection timed out");
        let is_network_error =
            stderr.contains("Connection refused") || stderr.contains("Could not resolve host");
        let is_404 = stderr.contains("404") || stderr.contains("Not Found");

        if is_timeout {
            tracing::debug!("ffmpeg timeout for {}: connection timed out", video_url);
        } else if is_network_error {
            tracing::debug!(
                "ffmpeg network error for {}: {}",
                video_url,
                stderr.lines().next().unwrap_or("unknown")
            );
        } else if is_404 {
            tracing::debug!("ffmpeg 404 error for {}: resource not found", video_url);
        } else {
            tracing::debug!(
                "ffmpeg failed for {}: {}",
                video_url,
                stderr.lines().take(3).collect::<Vec<_>>().join(" | ")
            );
        }

        metrics::record_ffmpeg_extraction(false);

        return Err(SvcError::Io(std::io::Error::other(format!(
            "ffmpeg failed: {}",
            stderr
        ))));
    }

    tracing::debug!("ffmpeg successfully extracted thumbnail for: {}", video_url);

    metrics::record_ffmpeg_extraction(true);

    let thumbnail_data = tokio::fs::read(output_path).await.map_err(|e| {
        error!("failed to read thumbnail output: {}", e);
        SvcError::Io(e)
    })?;

    Ok(thumbnail_data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_failure_classifies_not_found_as_missing() {
        assert_eq!(
            CandidateFailureClass::from_status(404),
            CandidateFailureClass::Missing
        );
    }

    #[tokio::test]
    async fn extract_video_thumbnail_skips_negatively_cached_candidate() {
        crate::init_crypto_provider();
        let cache = CandidateFailureCache::new(
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(60),
        );
        let url = format!("https://cdn.example.com/{}.mp4", "a".repeat(64));
        cache.remember(&url, CandidateFailureClass::Missing).await;
        let http = reqwest::Client::new();
        let semaphore = Arc::new(Semaphore::new(1));

        let result = extract_video_thumbnail(
            &url,
            &semaphore,
            &http,
            &[],
            &[],
            Some(&cache),
            Instant::now() + std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(1),
        )
        .await;

        assert!(matches!(result, Err(SvcError::UpstreamError(404))));
    }
}
