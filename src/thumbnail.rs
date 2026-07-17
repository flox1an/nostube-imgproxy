use std::{sync::Arc, time::Instant};
use tokio::sync::Semaphore;
use tracing::{error, info};

use crate::{
    blossom::extract_blossom_hash, error::SvcError, metrics, network_policy::validate_untrusted_url,
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
pub async fn extract_video_thumbnail(
    video_url: &str,
    semaphore: &Arc<Semaphore>,
    servers: &[String],
    discovered_urls: &[String],
    deadline: Instant,
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

    let mut last_error = SvcError::UpstreamError(404);

    for (idx, url) in candidates.iter().enumerate() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            last_error = SvcError::UpstreamError(504);
            break;
        }
        tracing::debug!(
            "thumbnail attempt {}/{}: {}",
            idx + 1,
            candidates.len(),
            url
        );
        match tokio::time::timeout(remaining, extract_thumbnail_with_ffmpeg(url)).await {
            Ok(Ok(bytes)) => {
                tracing::info!(
                    "✓ thumbnail {}/{} ({} bytes) from {}",
                    idx + 1,
                    candidates.len(),
                    bytes.len(),
                    url
                );
                return Ok(bytes);
            }
            Ok(Err(error)) => {
                tracing::debug!(
                    "✗ thumbnail {}/{} failed: {:?}",
                    idx + 1,
                    candidates.len(),
                    error
                );
                last_error = error;
            }
            Err(_) => {
                tracing::debug!("✗ thumbnail {}/{} timed out", idx + 1, candidates.len());
                last_error = SvcError::UpstreamError(504);
                break;
            }
        }
    }

    tracing::warn!(
        "all {} candidates exhausted for {}",
        candidates.len(),
        video_url
    );
    Err(last_error)
}

/// Extract a thumbnail from a video using ffmpeg CLI
async fn extract_thumbnail_with_ffmpeg(video_url: &str) -> Result<Vec<u8>, SvcError> {
    use tokio::process::Command;

    // Create a temporary file for the output
    let temp_file = tempfile::NamedTempFile::new().map_err(SvcError::Io)?;
    let output_path = temp_file.path();

    // Run ffmpeg to extract thumbnail
    // Equivalent to:
    // ffmpeg -ss 0.5 -i <video_url> -vframes 1 -vf "scale=-1:'min(720,ih)'" -q:v 80 -c:v libwebp -f image2 output.webp
    tracing::debug!("spawning ffmpeg for video: {}", video_url);

    let output = Command::new("ffmpeg")
        .args([
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
        .output()
        .await
        .map_err(|e| {
            error!("failed to spawn ffmpeg for {}: {}", video_url, e);
            SvcError::Io(e)
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _stdout = String::from_utf8_lossy(&output.stdout);

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

    // Read the generated thumbnail
    let thumbnail_data = tokio::fs::read(output_path).await.map_err(|e| {
        error!("failed to read thumbnail output: {}", e);
        SvcError::Io(e)
    })?;

    Ok(thumbnail_data)
}
