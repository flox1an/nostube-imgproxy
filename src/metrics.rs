use lazy_static::lazy_static;
use prometheus::{
    register_counter_vec, register_histogram_vec, CounterVec, Encoder, HistogramVec, TextEncoder,
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

    // FFmpeg extraction metrics
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
    FFMPEG_EXTRACTIONS_TOTAL.with_label_values(&[status]).inc();
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Counters are process-global and other tests may touch them, so every
    /// assertion here is a delta against a freshly sampled baseline. Each test
    /// uses labels unique to itself to stay independent of execution order.
    fn counter(vec: &CounterVec, labels: &[&str]) -> f64 {
        vec.with_label_values(labels).get()
    }

    #[test]
    fn record_http_request_increments_the_labelled_counter() {
        let labels = ["/metrics-test-a", "GET", "200"];
        let before = counter(&HTTP_REQUESTS_TOTAL, &labels);

        record_http_request("/metrics-test-a", "GET", 200);

        assert_eq!(counter(&HTTP_REQUESTS_TOTAL, &labels), before + 1.0);
    }

    #[test]
    fn record_http_request_separates_distinct_status_codes() {
        record_http_request("/metrics-test-b", "GET", 200);
        record_http_request("/metrics-test-b", "GET", 500);

        // Distinct statuses must not collapse into one series.
        assert!(counter(&HTTP_REQUESTS_TOTAL, &["/metrics-test-b", "GET", "200"]) >= 1.0);
        assert!(counter(&HTTP_REQUESTS_TOTAL, &["/metrics-test-b", "GET", "500"]) >= 1.0);
    }

    #[test]
    fn cache_hit_and_miss_counters_are_independent() {
        let hits_before = counter(&CACHE_HITS_TOTAL, &["metrics-test-c"]);
        let misses_before = counter(&CACHE_MISSES_TOTAL, &["metrics-test-c"]);

        record_cache_hit("metrics-test-c");

        assert_eq!(
            counter(&CACHE_HITS_TOTAL, &["metrics-test-c"]),
            hits_before + 1.0
        );
        assert_eq!(
            counter(&CACHE_MISSES_TOTAL, &["metrics-test-c"]),
            misses_before,
            "recording a hit must not move the miss counter"
        );
    }

    #[test]
    fn record_cache_miss_increments_only_misses() {
        let before = counter(&CACHE_MISSES_TOTAL, &["metrics-test-d"]);
        record_cache_miss("metrics-test-d");
        assert_eq!(
            counter(&CACHE_MISSES_TOTAL, &["metrics-test-d"]),
            before + 1.0
        );
    }

    #[test]
    fn image_and_video_counters_track_their_own_formats() {
        let img_before = counter(&IMAGES_PROCESSED_TOTAL, &["metrics-test-webp"]);
        let vid_before = counter(&VIDEOS_PROCESSED_TOTAL, &["metrics-test-webp"]);

        record_image_processed("metrics-test-webp");

        assert_eq!(
            counter(&IMAGES_PROCESSED_TOTAL, &["metrics-test-webp"]),
            img_before + 1.0
        );
        assert_eq!(
            counter(&VIDEOS_PROCESSED_TOTAL, &["metrics-test-webp"]),
            vid_before,
            "image processing must not count as video processing"
        );
    }

    #[test]
    fn record_video_processed_increments_the_video_counter() {
        let before = counter(&VIDEOS_PROCESSED_TOTAL, &["metrics-test-vid"]);
        record_video_processed("metrics-test-vid");
        assert_eq!(
            counter(&VIDEOS_PROCESSED_TOTAL, &["metrics-test-vid"]),
            before + 1.0
        );
    }

    #[test]
    fn ffmpeg_extraction_maps_the_boolean_to_a_status_label() {
        let success_before = counter(&FFMPEG_EXTRACTIONS_TOTAL, &["success"]);
        let failure_before = counter(&FFMPEG_EXTRACTIONS_TOTAL, &["failure"]);

        record_ffmpeg_extraction(true);
        record_ffmpeg_extraction(false);

        assert_eq!(
            counter(&FFMPEG_EXTRACTIONS_TOTAL, &["success"]),
            success_before + 1.0
        );
        assert_eq!(
            counter(&FFMPEG_EXTRACTIONS_TOTAL, &["failure"]),
            failure_before + 1.0
        );
    }

    #[test]
    fn byte_counters_accumulate_the_reported_amount() {
        let before = counter(&BYTES_DOWNLOADED_TOTAL, &["metrics-test-src"]);
        record_bytes_downloaded("metrics-test-src", 1500);
        record_bytes_downloaded("metrics-test-src", 500);
        assert_eq!(
            counter(&BYTES_DOWNLOADED_TOTAL, &["metrics-test-src"]),
            before + 2000.0
        );
    }

    #[test]
    fn record_bytes_served_accumulates_by_content_type() {
        let before = counter(&BYTES_SERVED_TOTAL, &["metrics-test-served"]);
        record_bytes_served("metrics-test-served", 42);
        assert_eq!(
            counter(&BYTES_SERVED_TOTAL, &["metrics-test-served"]),
            before + 42.0
        );
    }

    #[test]
    fn record_processing_error_increments_by_error_type() {
        let before = counter(&PROCESSING_ERRORS_TOTAL, &["metrics-test-decode"]);
        record_processing_error("metrics-test-decode");
        assert_eq!(
            counter(&PROCESSING_ERRORS_TOTAL, &["metrics-test-decode"]),
            before + 1.0
        );
    }

    #[test]
    fn observe_http_duration_records_a_histogram_sample() {
        let labels = ["/metrics-test-hist", "GET"];
        let before = HTTP_REQUEST_DURATION_SECONDS
            .with_label_values(&labels)
            .get_sample_count();

        observe_http_duration("/metrics-test-hist", "GET", 0.123);

        let after = HTTP_REQUEST_DURATION_SECONDS
            .with_label_values(&labels)
            .get_sample_count();
        assert_eq!(after, before + 1);
    }

    #[test]
    fn encode_metrics_emits_prometheus_text_for_recorded_series() {
        record_image_processed("metrics-test-encode");

        let encoded = encode_metrics().expect("metrics encode");

        // Prometheus text format: HELP/TYPE preamble plus the labelled sample.
        assert!(encoded.contains("# HELP imgproxy_images_processed_total"));
        assert!(encoded.contains("# TYPE imgproxy_images_processed_total counter"));
        assert!(
            encoded.contains(
                r#"imgproxy_images_processed_total{output_format="metrics-test-encode"}"#
            ),
            "recorded series missing from encoded output"
        );
    }
}
