use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::{error, info};

use crate::error::SvcError;

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

/// Extract a video thumbnail and return the image bytes (to be cached as "original")
pub async fn extract_video_thumbnail(
    video_url: &str,
    semaphore: &Arc<Semaphore>,
) -> Result<Vec<u8>, SvcError> {
    info!("extracting thumbnail from video: {}", video_url);

    // Acquire semaphore permit to limit concurrent ffmpeg processes
    // This will block (async-wait) if MAX_FFMPEG_CONCURRENT limit is reached
    // Automatically releases permit when _permit is dropped (when function returns)
    let _permit = semaphore
        .acquire()
        .await
        .map_err(|_| SvcError::Io(std::io::Error::new(std::io::ErrorKind::Other, "semaphore error")))?;

    extract_thumbnail_with_ffmpeg(video_url).await
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
            error!("failed to spawn ffmpeg: {}", e);
            SvcError::Io(e)
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        error!("ffmpeg failed: {}", stderr);
        return Err(SvcError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("ffmpeg failed: {}", stderr),
        )));
    }

    // Read the generated thumbnail
    let thumbnail_data = tokio::fs::read(output_path)
        .await
        .map_err(|e| {
            error!("failed to read thumbnail: {}", e);
            SvcError::Io(e)
        })?;

    Ok(thumbnail_data)
}

