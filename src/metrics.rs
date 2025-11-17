use lazy_static::lazy_static;
use prometheus::{
    register_counter_vec, register_gauge, register_histogram_vec, CounterVec, Gauge, HistogramVec,
    TextEncoder, Encoder,
};

lazy_static! {
    // HTTP request metrics
    pub static ref HTTP_REQUESTS_TOTAL: CounterVec = register_counter_vec!(
        "imgproxy_http_requests_total",
        "Total number of HTTP requests by endpoint and status",
        &["endpoint", "method", "status"]
    )
    .unwrap();

    pub static ref HTTP_REQUEST_DURATION_SECONDS: HistogramVec = register_histogram_vec!(
        "imgproxy_http_request_duration_seconds",
        "HTTP request latencies in seconds",
        &["endpoint", "method"],
        vec![0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]
    )
    .unwrap();

    // Cache metrics
    pub static ref CACHE_HITS_TOTAL: CounterVec = register_counter_vec!(
        "imgproxy_cache_hits_total",
        "Total number of cache hits by cache type",
        &["cache_type"]
    )
    .unwrap();

    pub static ref CACHE_MISSES_TOTAL: CounterVec = register_counter_vec!(
        "imgproxy_cache_misses_total",
        "Total number of cache misses by cache type",
        &["cache_type"]
    )
    .unwrap();

    // Processing metrics
    pub static ref IMAGES_PROCESSED_TOTAL: CounterVec = register_counter_vec!(
        "imgproxy_images_processed_total",
        "Total number of images processed by output format",
        &["output_format"]
    )
    .unwrap();

    pub static ref VIDEOS_PROCESSED_TOTAL: CounterVec = register_counter_vec!(
        "imgproxy_videos_processed_total",
        "Total number of video thumbnails extracted",
        &["output_format"]
    )
    .unwrap();

    pub static ref PROCESSING_ERRORS_TOTAL: CounterVec = register_counter_vec!(
        "imgproxy_processing_errors_total",
        "Total number of processing errors by type",
        &["error_type"]
    )
    .unwrap();

    // FFmpeg semaphore metrics
    pub static ref FFMPEG_SEMAPHORE_PERMITS_AVAILABLE: Gauge = register_gauge!(
        "imgproxy_ffmpeg_semaphore_permits_available",
        "Number of available FFmpeg semaphore permits"
    )
    .unwrap();

    pub static ref FFMPEG_SEMAPHORE_WAITERS: Gauge = register_gauge!(
        "imgproxy_ffmpeg_semaphore_waiters",
        "Number of tasks waiting for FFmpeg semaphore"
    )
    .unwrap();

    pub static ref FFMPEG_EXTRACTIONS_TOTAL: CounterVec = register_counter_vec!(
        "imgproxy_ffmpeg_extractions_total",
        "Total number of FFmpeg thumbnail extractions",
        &["status"]
    )
    .unwrap();

    // Bytes transferred metrics
    pub static ref BYTES_DOWNLOADED_TOTAL: CounterVec = register_counter_vec!(
        "imgproxy_bytes_downloaded_total",
        "Total bytes downloaded from source URLs",
        &["source_type"]
    )
    .unwrap();

    pub static ref BYTES_SERVED_TOTAL: CounterVec = register_counter_vec!(
        "imgproxy_bytes_served_total",
        "Total bytes served to clients",
        &["content_type"]
    )
    .unwrap();
}

/// Encode all metrics to Prometheus text format
pub fn encode_metrics() -> Result<String, Box<dyn std::error::Error>> {
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();
    encoder.encode(&metric_families, &mut buffer)?;
    Ok(String::from_utf8(buffer)?)
}

/// Record HTTP request
pub fn record_http_request(endpoint: &str, method: &str, status: u16) {
    HTTP_REQUESTS_TOTAL
        .with_label_values(&[endpoint, method, &status.to_string()])
        .inc();
}

/// Record HTTP request duration
pub fn observe_http_duration(endpoint: &str, method: &str, duration_secs: f64) {
    HTTP_REQUEST_DURATION_SECONDS
        .with_label_values(&[endpoint, method])
        .observe(duration_secs);
}

/// Record cache hit
pub fn record_cache_hit(cache_type: &str) {
    CACHE_HITS_TOTAL.with_label_values(&[cache_type]).inc();
}

/// Record cache miss
pub fn record_cache_miss(cache_type: &str) {
    CACHE_MISSES_TOTAL.with_label_values(&[cache_type]).inc();
}

/// Record image processed
pub fn record_image_processed(output_format: &str) {
    IMAGES_PROCESSED_TOTAL
        .with_label_values(&[output_format])
        .inc();
}

/// Record video processed
pub fn record_video_processed(output_format: &str) {
    VIDEOS_PROCESSED_TOTAL
        .with_label_values(&[output_format])
        .inc();
}

/// Record processing error
pub fn record_processing_error(error_type: &str) {
    PROCESSING_ERRORS_TOTAL
        .with_label_values(&[error_type])
        .inc();
}

/// Record FFmpeg extraction
pub fn record_ffmpeg_extraction(success: bool) {
    let status = if success { "success" } else { "failure" };
    FFMPEG_EXTRACTIONS_TOTAL
        .with_label_values(&[status])
        .inc();
}

/// Record bytes downloaded
pub fn record_bytes_downloaded(source_type: &str, bytes: usize) {
    BYTES_DOWNLOADED_TOTAL
        .with_label_values(&[source_type])
        .inc_by(bytes as f64);
}

/// Record bytes served
pub fn record_bytes_served(content_type: &str, bytes: usize) {
    BYTES_SERVED_TOTAL
        .with_label_values(&[content_type])
        .inc_by(bytes as f64);
}

/// Update FFmpeg semaphore metrics
pub fn update_ffmpeg_semaphore_metrics(permits_available: usize, waiters: usize) {
    FFMPEG_SEMAPHORE_PERMITS_AVAILABLE.set(permits_available as f64);
    FFMPEG_SEMAPHORE_WAITERS.set(waiters as f64);
}
