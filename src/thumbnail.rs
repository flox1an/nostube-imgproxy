use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::{error, info};

use crate::{error::SvcError, metrics};

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
    let url_lower = url.to_lowercase();
    url_lower.ends_with(".mp4")
        || url_lower.ends_with(".mov")
        || url_lower.ends_with(".avi")
        || url_lower.ends_with(".webm")
        || url_lower.ends_with(".mkv")
        || url_lower.ends_with(".flv")
        || url_lower.ends_with(".wmv")
        || url_lower.ends_with(".m4v")
        || url_lower.ends_with(".mpg")
        || url_lower.ends_with(".mpeg")
        || url_lower.ends_with(".3gp")
        || url_lower.ends_with(".ogv")
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

/// Extract a video thumbnail and return the image bytes (to be cached as "original")
pub async fn extract_video_thumbnail(
    video_url: &str,
    semaphore: &Arc<Semaphore>,
    blossom_fallback_servers: &[String],
) -> Result<Vec<u8>, SvcError> {
    info!("extracting thumbnail from video: {}", video_url);

    // Acquire semaphore permit to limit concurrent ffmpeg processes
    // This will block (async-wait) if MAX_FFMPEG_CONCURRENT limit is reached
    // Automatically releases permit when _permit is dropped (when function returns)
    let _permit = semaphore
        .acquire()
        .await
        .map_err(|_| SvcError::Io(std::io::Error::new(std::io::ErrorKind::Other, "semaphore error")))?;

    // Try original URL first
    let result = extract_thumbnail_with_ffmpeg(video_url).await;

    // Log success or failure of primary attempt
    match &result {
        Ok(bytes) => {
            tracing::debug!("primary server succeeded for video {}, extracted {} bytes", video_url, bytes.len());
            return Ok(bytes.clone());
        }
        Err(e) => {
            tracing::debug!("primary server failed for video {}: {:?}", video_url, e);
        }
    }

    // If failed and it's a Blossom URL, try fallback servers
    if is_blossom_url(video_url) {
        tracing::debug!("url is blossom format, attempting {} fallback servers", blossom_fallback_servers.len());

        if let Some((hash, ext)) = extract_blossom_hash(video_url) {
            for (idx, fallback_server) in blossom_fallback_servers.iter().enumerate() {
                let fallback_url = format!("{}/{}.{}", fallback_server.trim_end_matches('/'), hash, ext);
                tracing::debug!(
                    "attempting fallback server {}/{} for video: {}",
                    idx + 1,
                    blossom_fallback_servers.len(),
                    fallback_url
                );

                match extract_thumbnail_with_ffmpeg(&fallback_url).await {
                    Ok(thumbnail_bytes) => {
                        tracing::info!(
                            "✓ fallback server {} succeeded for video, extracted {} bytes from {}",
                            idx + 1,
                            thumbnail_bytes.len(),
                            fallback_server
                        );
                        return Ok(thumbnail_bytes);
                    }
                    Err(e) => {
                        tracing::debug!(
                            "✗ fallback server {} extraction failed for {}: {:?}",
                            idx + 1,
                            fallback_server,
                            e
                        );
                    }
                }
            }

            tracing::warn!(
                "all {} fallback servers exhausted for video {}, returning original error",
                blossom_fallback_servers.len(),
                video_url
            );
        }
    } else {
        tracing::debug!("url is not blossom format, skipping fallback servers");
    }

    result
}

/// Extract a thumbnail from a video using ffmpeg CLI
async fn extract_thumbnail_with_ffmpeg(video_url: &str) -> Result<Vec<u8>, SvcError> {
    use tokio::process::Command;
    
    // Create a temporary file for the output
    let temp_file = tempfile::NamedTempFile::new()
        .map_err(|e| SvcError::Io(e))?;
    let output_path = temp_file.path();

    // Run ffmpeg to extract thumbnail
    // Equivalent to:
    // ffmpeg -ss 0.5 -i <video_url> -vframes 1 -vf "scale=-1:'min(720,ih)'" -q:v 80 -c:v libwebp -f image2 output.webp
    tracing::debug!("spawning ffmpeg for video: {}", video_url);

    let output = Command::new("ffmpeg")
        .args(&[
            "-ss", "0.5",               // Seek to 0.5 seconds
            "-i", video_url,            // Input URL
            "-vframes", "1",            // Extract 1 frame
            "-vf", "scale=-1:'min(720,ih)'",  // Scale to max height 720, keep aspect ratio
            "-q:v", "80",               // Quality 80
            "-c:v", "libwebp",          // WebP codec
            "-f", "image2",             // Image format
            "-y",                       // Overwrite output file
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
        let is_network_error = stderr.contains("Connection refused") || stderr.contains("Could not resolve host");
        let is_404 = stderr.contains("404") || stderr.contains("Not Found");

        if is_timeout {
            tracing::debug!("ffmpeg timeout for {}: connection timed out", video_url);
        } else if is_network_error {
            tracing::debug!("ffmpeg network error for {}: {}", video_url, stderr.lines().next().unwrap_or("unknown"));
        } else if is_404 {
            tracing::debug!("ffmpeg 404 error for {}: resource not found", video_url);
        } else {
            tracing::debug!("ffmpeg failed for {}: {}", video_url, stderr.lines().take(3).collect::<Vec<_>>().join(" | "));
        }

        metrics::record_ffmpeg_extraction(false);

        return Err(SvcError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("ffmpeg failed: {}", stderr),
        )));
    }

    tracing::debug!("ffmpeg successfully extracted thumbnail for: {}", video_url);

    metrics::record_ffmpeg_extraction(true);

    // Read the generated thumbnail
    let thumbnail_data = tokio::fs::read(output_path)
        .await
        .map_err(|e| {
            error!("failed to read thumbnail: {}", e);
            SvcError::Io(e)
        })?;

    Ok(thumbnail_data)
}

