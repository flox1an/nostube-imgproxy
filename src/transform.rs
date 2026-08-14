use image::{imageops::FilterType, DynamicImage, GenericImageView, ImageFormat, Limits};
use percent_encoding::percent_decode_str;
use rgb::FromSlice;

use crate::error::SvcError;

#[derive(Debug, Clone)]
pub struct Directives {
    pub out_fmt: OutFmt,
    pub quality: u8,
    pub resize: Resize,
}

#[derive(Debug, Clone)]
pub enum OutFmt {
    Jpeg,
    Png,
    Webp,
    Avif,
}

impl OutFmt {
    pub fn mime_type(&self) -> &'static str {
        match self {
            OutFmt::Jpeg => "image/jpeg",
            OutFmt::Png => "image/png",
            OutFmt::Webp => "image/webp",
            OutFmt::Avif => "image/avif",
        }
    }

    pub fn extension(&self) -> &'static str {
        match self {
            OutFmt::Jpeg => "jpg",
            OutFmt::Png => "png",
            OutFmt::Webp => "webp",
            OutFmt::Avif => "avif",
        }
    }

    /// Short, stable name used as a metrics label.
    pub fn label(&self) -> &'static str {
        match self {
            OutFmt::Jpeg => "jpeg",
            OutFmt::Png => "png",
            OutFmt::Webp => "webp",
            OutFmt::Avif => "avif",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Resize {
    pub mode: ResizeMode,
    pub w: u32,
    pub h: u32,
}

impl Resize {
    /// Reject an out-of-range target before anything is fetched.
    ///
    /// The ceiling is enforced again at the allocation site in
    /// [`resize_checked`], but a request asking for a 60000² output should cost
    /// us nothing at all — not a `MAX_IMAGE_BYTES` download first.
    pub fn validate(&self, max_dimension: u32) -> Result<(), SvcError> {
        if self.w > max_dimension || self.h > max_dimension {
            return Err(SvcError::BadRequest("resize target too large"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub enum ResizeMode {
    Fit,
    Fill,
    FillDown,
    Force,
    Auto,
}

impl ResizeMode {
    /// Short, stable text form used in derivative cache keys.
    ///
    /// Changing a label invalidates every existing cache entry for that mode,
    /// so these strings are part of the key format and must stay stable.
    pub fn label(&self) -> &'static str {
        match self {
            ResizeMode::Fit => "fit",
            ResizeMode::Fill => "fill",
            ResizeMode::FillDown => "fill-down",
            ResizeMode::Force => "force",
            ResizeMode::Auto => "auto",
        }
    }
}

/// Parse URL path segments into directives and source URL
pub fn parse_rest(rest: &str) -> Result<(Directives, String), SvcError> {
    // Split at "/plain/"
    let (before_plain, after_plain) = rest
        .split_once("/plain/")
        .ok_or(SvcError::BadRequest("missing /plain/ segment"))?;

    // Directives are path segments between the leading "insecure/" and "/plain/"
    let segments: Vec<&str> = before_plain
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    // Defaults
    let mut out_fmt = OutFmt::Jpeg;
    let mut quality: u8 = 82; // sensible default similar to imgproxy defaults
    let mut resize = Resize {
        mode: ResizeMode::Fit,
        w: 0,
        h: 0,
    };

    for seg in segments {
        if let Some(arg) = seg.strip_prefix("f:") {
            out_fmt = match arg.to_ascii_lowercase().as_str() {
                "jpeg" | "jpg" => OutFmt::Jpeg,
                "png" => OutFmt::Png,
                "webp" => OutFmt::Webp,
                "avif" => OutFmt::Avif,
                _ => return Err(SvcError::BadRequest("unsupported format")),
            };
        } else if let Some(arg) = seg.strip_prefix("q:") {
            quality = arg
                .parse()
                .ok()
                .filter(|q: &u8| (1..=100).contains(q))
                .ok_or(SvcError::BadRequest("bad quality"))?;
        } else if let Some(arg) = seg.strip_prefix("rs:") {
            // Parse rs:<mode>:<w>:<h> or rt:<mode>:<w>:<h>
            resize = parse_resize_directive(arg)?;
        } else if let Some(arg) = seg.strip_prefix("rt:") {
            // Alternative syntax: rt:<mode>:<w>:<h>
            resize = parse_resize_directive(arg)?;
        }
    }

    // At least one dimension must be specified
    if resize.w == 0 && resize.h == 0 {
        return Err(SvcError::BadRequest("at least one dimension required"));
    }

    // Decode percent-encoded source URL
    let src_url = percent_decode_str(after_plain)
        .decode_utf8()
        .map_err(|_| SvcError::BadRequest("bad encoded url"))?
        .to_string();

    Ok((
        Directives {
            out_fmt,
            quality,
            resize,
        },
        src_url,
    ))
}

/// Parse a resize directive like "fill:480:480", "fit:800:600", "fit::600", or "fit:800:"
///
/// Shared with the `/thumb` query parser so there is exactly one resize grammar;
/// a second copy would let a cap added here be bypassed through the other route.
pub fn parse_resize_directive(arg: &str) -> Result<Resize, SvcError> {
    let parts: Vec<&str> = arg.split(':').collect();
    if parts.len() != 3 {
        return Err(SvcError::BadRequest("invalid resize format"));
    }

    let mode = match parts[0].to_ascii_lowercase().as_str() {
        "fit" => ResizeMode::Fit,
        "fill" => ResizeMode::Fill,
        "fill-down" => ResizeMode::FillDown,
        "force" => ResizeMode::Force,
        "auto" => ResizeMode::Auto,
        _ => return Err(SvcError::BadRequest("unsupported resize mode")),
    };

    // Parse width and height, allowing empty strings (0 means "calculate from aspect ratio")
    let w: u32 = if parts[1].is_empty() {
        0
    } else {
        parts[1]
            .parse()
            .map_err(|_| SvcError::BadRequest("bad width"))?
    };

    let h: u32 = if parts[2].is_empty() {
        0
    } else {
        parts[2]
            .parse()
            .map_err(|_| SvcError::BadRequest("bad height"))?
    };

    Ok(Resize { mode, w, h })
}

/// Apply resize transformation based on the resize mode.
///
/// `limits` is the same budget the decoder was handed, reused here because the
/// resize step is what actually allocates the most: `imageops::resize` builds an
/// `Rgba32F` intermediate at 16 bytes per pixel that `Limits::max_alloc` never
/// sees. Without this check a legal-looking request asks for gigabytes, and a
/// failed allocation is an `abort()` — not a panic, so `CpuPool` cannot contain
/// it.
pub fn apply_resize(
    img: DynamicImage,
    resize: &Resize,
    limits: &Limits,
) -> Result<DynamicImage, SvcError> {
    let (src_w, src_h) = img.dimensions();
    if src_w == 0 || src_h == 0 {
        return Err(SvcError::BadRequest("source image has a zero dimension"));
    }

    // Calculate missing dimension based on aspect ratio
    let (target_w, target_h) = calculate_dimensions(src_w, src_h, resize.w, resize.h);

    // Determine the actual mode for 'auto'
    let mode = match resize.mode {
        ResizeMode::Auto => {
            let src_portrait = src_h > src_w;
            let target_portrait = target_h > target_w;
            if src_portrait == target_portrait {
                ResizeMode::Fill
            } else {
                ResizeMode::Fit
            }
        }
        ref m => m.clone(),
    };

    match mode {
        ResizeMode::Fit => apply_resize_fit(img, target_w, target_h, limits),
        ResizeMode::Fill => apply_resize_fill(img, target_w, target_h, limits),
        ResizeMode::FillDown => apply_resize_fill_down(img, target_w, target_h, limits),
        ResizeMode::Force => apply_resize_force(img, target_w, target_h, limits),
        ResizeMode::Auto => unreachable!(), // Already resolved above
    }
}

/// The one place a resize actually allocates, and therefore the one place the
/// allocation is budgeted.
///
/// `imageops::resize` allocates an `Rgba32F` intermediate of `src_w × new_h` at
/// 16 bytes per pixel regardless of the source colour type, plus the destination
/// buffer. Neither is covered by `Limits::max_alloc`, which only bounds the
/// decoder's own framebuffer.
fn resize_checked(
    img: DynamicImage,
    new_w: u32,
    new_h: u32,
    limits: &Limits,
) -> Result<DynamicImage, SvcError> {
    if new_w == 0 || new_h == 0 {
        return Err(SvcError::BadRequest("resize target rounds to zero"));
    }

    let max_dimension = max_output_dimension(limits);
    if new_w > max_dimension || new_h > max_dimension {
        return Err(SvcError::BadRequest("resize target too large"));
    }

    if let Some(budget) = limits.max_alloc {
        let (src_w, _) = img.dimensions();
        let intermediate = u64::from(src_w) * u64::from(new_h) * 16;
        // 16 B/px is the widest `DynamicImage` variant, so this over-estimates
        // for the common RGBA8 case rather than under-estimating for Rgba32F.
        let destination = u64::from(new_w) * u64::from(new_h) * 16;
        if intermediate.saturating_add(destination) > budget {
            return Err(SvcError::BadRequest(
                "resize would exceed the memory budget",
            ));
        }
    }

    Ok(img.resize_exact(new_w, new_h, FilterType::Lanczos3))
}

/// Per-axis ceiling for a resize target, derived from the decoder limits so
/// `MAX_IMAGE_DIMENSION` stays the single source of truth.
fn max_output_dimension(limits: &Limits) -> u32 {
    limits
        .max_image_width
        .unwrap_or(u32::MAX)
        .min(limits.max_image_height.unwrap_or(u32::MAX))
}

/// Calculate target dimensions, filling in missing dimension based on aspect ratio
fn calculate_dimensions(src_w: u32, src_h: u32, target_w: u32, target_h: u32) -> (u32, u32) {
    match (target_w, target_h) {
        (0, 0) => (src_w, src_h), // Both 0: keep original (shouldn't happen due to validation)
        (0, h) => {
            // Width is 0: calculate from height maintaining aspect ratio
            let aspect = src_w as f32 / src_h as f32;
            let w = (h as f32 * aspect).round() as u32;
            (w, h)
        }
        (w, 0) => {
            // Height is 0: calculate from width maintaining aspect ratio
            let aspect = src_h as f32 / src_w as f32;
            let h = (w as f32 * aspect).round() as u32;
            (w, h)
        }
        (w, h) => (w, h), // Both specified: use as-is
    }
}

/// Fit: Resize while keeping aspect ratio to fit within the given size
fn apply_resize_fit(
    img: DynamicImage,
    target_w: u32,
    target_h: u32,
    limits: &Limits,
) -> Result<DynamicImage, SvcError> {
    let (w, h) = img.dimensions();

    // Scale to fit within the box
    let scale = f32::min(target_w as f32 / w as f32, target_h as f32 / h as f32);

    // Don't upscale if image is smaller
    let scale = f32::min(scale, 1.0);

    // A degenerate aspect ratio rounds the short axis to zero, which every
    // encoder rejects — one pixel is the smallest meaningful output.
    let new_w = (w as f32 * scale).round().max(1.0) as u32;
    let new_h = (h as f32 * scale).round().max(1.0) as u32;

    resize_checked(img, new_w, new_h, limits)
}

/// Fill: Resize while keeping aspect ratio to fill the given size, with center crop
fn apply_resize_fill(
    img: DynamicImage,
    target_w: u32,
    target_h: u32,
    limits: &Limits,
) -> Result<DynamicImage, SvcError> {
    let (w, h) = img.dimensions();

    // Scale to fill the box
    let scale = f32::max(target_w as f32 / w as f32, target_h as f32 / h as f32);
    let new_w = (w as f32 * scale).ceil().max(1.0) as u32;
    let new_h = (h as f32 * scale).ceil().max(1.0) as u32;

    let resized = resize_checked(img, new_w, new_h, limits)?;

    // Center crop
    let x = (new_w.saturating_sub(target_w)) / 2;
    let y = (new_h.saturating_sub(target_h)) / 2;
    Ok(resized.crop_imm(x, y, target_w, target_h))
}

/// Fill-Down: Like fill, but if result is smaller, crop to maintain aspect ratio
fn apply_resize_fill_down(
    img: DynamicImage,
    target_w: u32,
    target_h: u32,
    limits: &Limits,
) -> Result<DynamicImage, SvcError> {
    let (w, h) = img.dimensions();

    // Scale to fill the box
    let scale = f32::max(target_w as f32 / w as f32, target_h as f32 / h as f32);

    // Don't upscale
    let scale = f32::min(scale, 1.0);

    let new_w = (w as f32 * scale).ceil().max(1.0) as u32;
    let new_h = (h as f32 * scale).ceil().max(1.0) as u32;

    let resized = resize_checked(img, new_w, new_h, limits)?;

    // If smaller than target, crop to maintain aspect ratio
    let crop_w = new_w.min(target_w);
    let crop_h = new_h.min(target_h);

    // Center crop
    let x = (new_w.saturating_sub(crop_w)) / 2;
    let y = (new_h.saturating_sub(crop_h)) / 2;
    Ok(resized.crop_imm(x, y, crop_w, crop_h))
}

/// Force: Resize without keeping aspect ratio
fn apply_resize_force(
    img: DynamicImage,
    target_w: u32,
    target_h: u32,
    limits: &Limits,
) -> Result<DynamicImage, SvcError> {
    resize_checked(img, target_w, target_h, limits)
}

/// Largest axis libwebp accepts (`WEBP_MAX_DIMENSION`). The `webp` crate's
/// `encode` unwraps the resulting error, so exceeding it panics the encode
/// thread instead of failing the request.
const MAX_WEBP_DIMENSION: u32 = 16_383;

/// Encode image to the specified format with quality settings
pub fn encode_image(img: &DynamicImage, fmt: &OutFmt, quality: u8) -> Result<Vec<u8>, SvcError> {
    let mut out = Vec::new();
    match fmt {
        OutFmt::Jpeg => {
            let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
            enc.encode_image(img)?;
        }
        OutFmt::Png => {
            let enc = image::codecs::png::PngEncoder::new(&mut out);
            img.write_with_encoder(enc)?;
        }
        OutFmt::Webp => {
            // `Encoder::from_image` refuses Luma and 16-bit variants outright,
            // and `encode` unwraps libwebp's error — an axis over
            // `WEBP_MAX_DIMENSION` would panic the worker. Normalising to RGBA8
            // and using the fallible entry point removes both failure modes.
            let rgba = img.to_rgba8();
            let (w, h) = (rgba.width(), rgba.height());
            if w == 0 || h == 0 || w > MAX_WEBP_DIMENSION || h > MAX_WEBP_DIMENSION {
                return Err(SvcError::BadRequest("output size unsupported by webp"));
            }
            let webp_data = webp::Encoder::from_rgba(rgba.as_raw(), w, h)
                .encode_simple(false, quality as f32)
                .map_err(|error| {
                    SvcError::Io(std::io::Error::other(format!(
                        "webp encode error: {error:?}"
                    )))
                })?;
            out.extend_from_slice(&webp_data);
        }
        OutFmt::Avif => {
            let rgba = img.to_rgba8();
            let (w, h) = (rgba.width(), rgba.height());

            // `as_rgba` reinterprets the existing buffer in place. Collecting
            // into a `Vec<rgb::RGBA<u8>>` instead would allocate and copy a
            // second full framebuffer for no benefit.
            let avif_img = ravif::Img::new(rgba.as_raw().as_rgba(), w as usize, h as usize);
            let encoder = ravif::Encoder::new()
                // `ravif` asserts `1.0..=100.0`; a `q:0` request must not be
                // able to panic the encode thread.
                .with_quality(quality.clamp(1, 100) as f32)
                .with_speed(6)
                // rav1e defaults to the whole global rayon pool (one thread
                // per core), so `cpu_concurrency` CPU permits would each fan
                // out to the full core count — cores² runnable threads and
                // the cpu.rs semaphore stops accounting for real work. One
                // permit must equal one core. (`with_num_threads` exists
                // because `ravif` is built with its `threading` feature.)
                .with_num_threads(Some(1));
            let encoded = encoder.encode_rgba(avif_img).map_err(|e| {
                SvcError::Io(std::io::Error::other(format!("AVIF encode error: {}", e)))
            })?;
            out.extend_from_slice(&encoded.avif_file);
        }
    }
    Ok(out)
}

/// Decode `bytes` with the format guessed from content and `limits` enforced.
///
/// `MAX_IMAGE_BYTES` bounds only the compressed payload; without these limits a
/// small file declaring enormous dimensions still gets a matching framebuffer
/// allocated, which is the cheapest way to OOM a memory-capped edge node.
pub fn decode_image(bytes: &[u8], limits: Limits) -> Result<DynamicImage, SvcError> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| SvcError::Decode(image::ImageError::IoError(e)))?;
    // Explicit input allowlist — not just a build-time feature list. Feature
    // unification means some other crate in the graph can silently re-enable
    // `image`'s default-formats (or a future decoder) without touching this
    // manifest; and `ImageFormat` keeps every enum variant regardless of which
    // decoders are compiled in. A format not on this list is refused *before*
    // `decode()` runs, so it can never allocate a framebuffer here.
    if !matches!(
        reader.format(),
        Some(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP)
    ) {
        return Err(SvcError::BadRequest("unsupported source format"));
    }
    reader.limits(limits);
    Ok(reader.decode()?)
}

/// The full synchronous decode → resize → encode pipeline.
///
/// Deliberately blocking and self-contained: callers hand this to
/// [`crate::cpu::CpuPool`] so it never runs on an async worker thread.
pub fn process_image(bytes: &[u8], dirs: &Directives, limits: Limits) -> Result<Vec<u8>, SvcError> {
    let img = decode_image(bytes, limits.clone())?;
    let img = apply_resize(img, &dirs.resize, &limits)?;
    encode_image(&img, &dirs.out_fmt, dirs.quality)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, RgbaImage};

    /// Solid-colour test image; content is irrelevant, geometry is what matters.
    fn img(w: u32, h: u32) -> DynamicImage {
        DynamicImage::ImageRgba8(RgbaImage::from_pixel(w, h, image::Rgba([10, 20, 30, 255])))
    }

    fn directives(rest: &str) -> Directives {
        parse_rest(rest).expect("directives parse").0
    }

    /// Production limits: the same shape `AppCfg::decode_limits` builds.
    fn test_limits() -> Limits {
        let mut limits = Limits::default();
        limits.max_image_width = Some(16_384);
        limits.max_image_height = Some(16_384);
        limits.max_alloc = Some(256 * 1024 * 1024);
        limits
    }

    /// Resize under production limits; these tests pin geometry, not the budget.
    fn resized(img: DynamicImage, resize: &Resize) -> DynamicImage {
        apply_resize(img, resize, &test_limits()).expect("resize must succeed")
    }

    // -----------------------------------------------------------------------
    // parse_rest — URL directive parsing
    // -----------------------------------------------------------------------

    #[test]
    fn parse_rest_requires_a_plain_segment() {
        let err = parse_rest("rs:fit:10:10/https://example.com/a.png").unwrap_err();
        assert!(matches!(
            err,
            SvcError::BadRequest("missing /plain/ segment")
        ));
    }

    #[test]
    fn parse_rest_extracts_source_url_after_plain() {
        let (_, src) = parse_rest("rs:fit:10:10/plain/https://example.com/a.png").unwrap();
        assert_eq!(src, "https://example.com/a.png");
    }

    #[test]
    fn parse_rest_percent_decodes_source_url() {
        let (_, src) =
            parse_rest("rs:fit:10:10/plain/https%3A%2F%2Fexample.com%2Fa%20b.png").unwrap();
        assert_eq!(src, "https://example.com/a b.png");
    }

    #[test]
    fn parse_rest_defaults_to_jpeg_quality_82_when_unspecified() {
        let d = directives("rs:fit:10:10/plain/https://example.com/a.png");
        assert!(matches!(d.out_fmt, OutFmt::Jpeg));
        assert_eq!(d.quality, 82);
    }

    #[test]
    fn parse_rest_maps_every_supported_format_token() {
        for (token, expected_mime) in [
            ("jpeg", "image/jpeg"),
            ("jpg", "image/jpeg"),
            ("png", "image/png"),
            ("webp", "image/webp"),
            ("avif", "image/avif"),
        ] {
            let d = directives(&format!("f:{token}/rs:fit:10:10/plain/https://e.com/a.png"));
            assert_eq!(d.out_fmt.mime_type(), expected_mime, "token {token}");
        }
    }

    #[test]
    fn parse_rest_format_token_is_case_insensitive() {
        let d = directives("f:PNG/rs:fit:10:10/plain/https://e.com/a.png");
        assert!(matches!(d.out_fmt, OutFmt::Png));
    }

    #[test]
    fn parse_rest_rejects_unsupported_format() {
        let err = parse_rest("f:gif/rs:fit:10:10/plain/https://e.com/a.gif").unwrap_err();
        assert!(matches!(err, SvcError::BadRequest("unsupported format")));
    }

    #[test]
    fn parse_rest_accepts_quality_at_the_upper_bound() {
        let d = directives("q:100/rs:fit:10:10/plain/https://e.com/a.png");
        assert_eq!(d.quality, 100);
    }

    #[test]
    fn parse_rest_rejects_quality_above_one_hundred() {
        let err = parse_rest("q:101/rs:fit:10:10/plain/https://e.com/a.png").unwrap_err();
        assert!(matches!(err, SvcError::BadRequest("bad quality")));
    }

    #[test]
    fn parse_rest_rejects_non_numeric_quality() {
        let err = parse_rest("q:high/rs:fit:10:10/plain/https://e.com/a.png").unwrap_err();
        assert!(matches!(err, SvcError::BadRequest("bad quality")));
    }

    #[test]
    fn parse_rest_requires_at_least_one_resize_dimension() {
        // No rs: directive at all leaves both dimensions at zero.
        let err = parse_rest("f:png/plain/https://e.com/a.png").unwrap_err();
        assert!(matches!(
            err,
            SvcError::BadRequest("at least one dimension required")
        ));

        // An explicit empty-empty resize is equally invalid.
        let err = parse_rest("rs:fit::/plain/https://e.com/a.png").unwrap_err();
        assert!(matches!(
            err,
            SvcError::BadRequest("at least one dimension required")
        ));
    }

    #[test]
    fn parse_rest_accepts_rt_as_an_alias_for_rs() {
        let d = directives("rt:fill:64:48/plain/https://e.com/a.png");
        assert!(matches!(d.resize.mode, ResizeMode::Fill));
        assert_eq!((d.resize.w, d.resize.h), (64, 48));
    }

    #[test]
    fn parse_rest_maps_every_supported_resize_mode() {
        assert!(matches!(
            directives("rs:fit:8:8/plain/u").resize.mode,
            ResizeMode::Fit
        ));
        assert!(matches!(
            directives("rs:fill:8:8/plain/u").resize.mode,
            ResizeMode::Fill
        ));
        assert!(matches!(
            directives("rs:fill-down:8:8/plain/u").resize.mode,
            ResizeMode::FillDown
        ));
        assert!(matches!(
            directives("rs:force:8:8/plain/u").resize.mode,
            ResizeMode::Force
        ));
        assert!(matches!(
            directives("rs:auto:8:8/plain/u").resize.mode,
            ResizeMode::Auto
        ));
    }

    #[test]
    fn parse_rest_resize_mode_is_case_insensitive() {
        assert!(matches!(
            directives("rs:FILL:8:8/plain/u").resize.mode,
            ResizeMode::Fill
        ));
    }

    #[test]
    fn parse_rest_treats_a_blank_dimension_as_aspect_driven() {
        let only_h = directives("rs:fit::600/plain/https://e.com/a.png").resize;
        assert_eq!((only_h.w, only_h.h), (0, 600));

        let only_w = directives("rs:fit:800:/plain/https://e.com/a.png").resize;
        assert_eq!((only_w.w, only_w.h), (800, 0));
    }

    #[test]
    fn parse_rest_rejects_malformed_resize_directives() {
        for (rest, expected) in [
            ("rs:fit:10/plain/u", "invalid resize format"),
            ("rs:fit:10:10:10/plain/u", "invalid resize format"),
            ("rs:squish:10:10/plain/u", "unsupported resize mode"),
            ("rs:fit:wide:10/plain/u", "bad width"),
            ("rs:fit:10:tall/plain/u", "bad height"),
        ] {
            match parse_rest(rest).unwrap_err() {
                SvcError::BadRequest(msg) => assert_eq!(msg, expected, "for {rest}"),
                other => panic!("expected BadRequest for {rest}, got {other:?}"),
            }
        }
    }

    #[test]
    fn parse_rest_lets_the_last_repeated_directive_win() {
        let d = directives("f:png/f:webp/rs:fit:10:10/plain/https://e.com/a.png");
        assert!(matches!(d.out_fmt, OutFmt::Webp));
    }

    #[test]
    fn parse_rest_ignores_unknown_directive_segments() {
        let d = directives("bogus:1/rs:fit:10:10/plain/https://e.com/a.png");
        assert_eq!((d.resize.w, d.resize.h), (10, 10));
    }

    // -----------------------------------------------------------------------
    // OutFmt
    // -----------------------------------------------------------------------

    #[test]
    fn out_fmt_extension_matches_mime_family() {
        for (fmt, ext, mime) in [
            (OutFmt::Jpeg, "jpg", "image/jpeg"),
            (OutFmt::Png, "png", "image/png"),
            (OutFmt::Webp, "webp", "image/webp"),
            (OutFmt::Avif, "avif", "image/avif"),
        ] {
            assert_eq!(fmt.extension(), ext);
            assert_eq!(fmt.mime_type(), mime);
        }
    }

    // -----------------------------------------------------------------------
    // apply_resize
    // -----------------------------------------------------------------------

    #[test]
    fn fit_resize_preserves_aspect_within_the_box() {
        let out = resized(
            img(400, 200),
            &Resize {
                mode: ResizeMode::Fit,
                w: 100,
                h: 100,
            },
        );
        // Scale is limited by width (100/400), so height follows the 2:1 ratio.
        assert_eq!(out.dimensions(), (100, 50));
    }

    #[test]
    fn fit_resize_never_upscales_a_smaller_source() {
        let out = resized(
            img(50, 40),
            &Resize {
                mode: ResizeMode::Fit,
                w: 500,
                h: 400,
            },
        );
        assert_eq!(out.dimensions(), (50, 40));
    }

    #[test]
    fn fill_resize_returns_exactly_the_requested_box() {
        let out = resized(
            img(400, 200),
            &Resize {
                mode: ResizeMode::Fill,
                w: 100,
                h: 100,
            },
        );
        assert_eq!(out.dimensions(), (100, 100));
    }

    #[test]
    fn fill_resize_upscales_to_cover_a_larger_box() {
        let out = resized(
            img(50, 50),
            &Resize {
                mode: ResizeMode::Fill,
                w: 200,
                h: 200,
            },
        );
        assert_eq!(out.dimensions(), (200, 200));
    }

    #[test]
    fn fill_down_resize_does_not_upscale_and_clamps_to_source() {
        let out = resized(
            img(80, 60),
            &Resize {
                mode: ResizeMode::FillDown,
                w: 400,
                h: 300,
            },
        );
        // Upscaling is forbidden, so the crop clamps to the source size.
        assert_eq!(out.dimensions(), (80, 60));
    }

    #[test]
    fn force_resize_ignores_aspect_ratio() {
        let out = resized(
            img(400, 200),
            &Resize {
                mode: ResizeMode::Force,
                w: 90,
                h: 90,
            },
        );
        assert_eq!(out.dimensions(), (90, 90));
    }

    #[test]
    fn auto_resize_fills_when_orientation_matches() {
        // Landscape source, landscape target -> behaves like fill (exact box).
        let out = resized(
            img(400, 200),
            &Resize {
                mode: ResizeMode::Auto,
                w: 200,
                h: 100,
            },
        );
        assert_eq!(out.dimensions(), (200, 100));
    }

    #[test]
    fn auto_resize_fits_when_orientation_differs() {
        // Landscape source, portrait target -> behaves like fit (aspect kept).
        let out = resized(
            img(400, 200),
            &Resize {
                mode: ResizeMode::Auto,
                w: 100,
                h: 200,
            },
        );
        assert_eq!(out.dimensions(), (100, 50));
    }

    #[test]
    fn missing_height_is_derived_from_source_aspect() {
        let out = resized(
            img(400, 200),
            &Resize {
                mode: ResizeMode::Force,
                w: 100,
                h: 0,
            },
        );
        assert_eq!(out.dimensions(), (100, 50));
    }

    #[test]
    fn missing_width_is_derived_from_source_aspect() {
        let out = resized(
            img(400, 200),
            &Resize {
                mode: ResizeMode::Force,
                w: 0,
                h: 50,
            },
        );
        assert_eq!(out.dimensions(), (100, 50));
    }

    // -----------------------------------------------------------------------
    // encode_image
    // -----------------------------------------------------------------------

    #[test]
    fn encode_image_emits_jpeg_magic_bytes() {
        let out = encode_image(&img(16, 16), &OutFmt::Jpeg, 80).unwrap();
        assert_eq!(&out[..2], &[0xFF, 0xD8], "JPEG SOI marker");
    }

    #[test]
    fn encode_image_emits_png_magic_bytes() {
        let out = encode_image(&img(16, 16), &OutFmt::Png, 80).unwrap();
        assert_eq!(&out[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn encode_image_emits_webp_container() {
        let out = encode_image(&img(16, 16), &OutFmt::Webp, 80).unwrap();
        assert_eq!(&out[..4], b"RIFF");
        assert_eq!(&out[8..12], b"WEBP");
    }

    #[test]
    fn encode_image_emits_avif_brand() {
        let out = encode_image(&img(16, 16), &OutFmt::Avif, 60).unwrap();
        // ISO-BMFF: 4-byte box size, then "ftyp", then the "avif" brand.
        assert_eq!(&out[4..8], b"ftyp");
        assert_eq!(&out[8..12], b"avif");
    }

    #[test]
    fn encode_image_round_trips_to_the_requested_dimensions() {
        for fmt in [OutFmt::Jpeg, OutFmt::Png, OutFmt::Webp] {
            let bytes = encode_image(&img(24, 12), &fmt, 90).unwrap();
            let decoded = image::load_from_memory(&bytes)
                .unwrap_or_else(|e| panic!("decode {} failed: {e}", fmt.extension()));
            assert_eq!(
                decoded.dimensions(),
                (24, 12),
                "dimensions lost for {}",
                fmt.extension()
            );
        }
    }

    #[test]
    fn lower_jpeg_quality_produces_smaller_output() {
        // Use a noisy image so quality actually influences the encoded size.
        let mut noisy = RgbaImage::new(64, 64);
        for (x, y, pixel) in noisy.enumerate_pixels_mut() {
            let v = ((x * 7 + y * 13) % 256) as u8;
            *pixel = image::Rgba([v, v.wrapping_mul(3), v.wrapping_add(97), 255]);
        }
        let noisy = DynamicImage::ImageRgba8(noisy);

        let low = encode_image(&noisy, &OutFmt::Jpeg, 20).unwrap();
        let high = encode_image(&noisy, &OutFmt::Jpeg, 95).unwrap();
        assert!(
            low.len() < high.len(),
            "q20 ({} bytes) should be smaller than q95 ({} bytes)",
            low.len(),
            high.len()
        );
    }
    // -----------------------------------------------------------------------
    // resize and encode guards
    // -----------------------------------------------------------------------

    #[test]
    fn resize_rejects_a_target_beyond_the_dimension_cap() {
        // `rs:force:60000:60000` allocated 14.4 GB and died in
        // `handle_alloc_error`, which is an abort the CpuPool cannot contain.
        let error = apply_resize(
            img(64, 64),
            &Resize {
                mode: ResizeMode::Force,
                w: 60_000,
                h: 60_000,
            },
            &test_limits(),
        )
        .expect_err("an oversized target must be refused");
        assert!(matches!(
            error,
            SvcError::BadRequest("resize target too large")
        ));
    }

    #[test]
    fn resize_rejects_an_intermediate_buffer_over_the_alloc_budget() {
        // `imageops::resize` builds an Rgba32F intermediate at 16 B/px that
        // `Limits::max_alloc` never sees, so a scale-1.0 `fit` on a large source
        // busts the budget without any upscaling at all.
        let mut limits = test_limits();
        limits.max_alloc = Some(64 * 1024);

        let error = apply_resize(
            img(800, 600),
            &Resize {
                mode: ResizeMode::Fit,
                w: 800,
                h: 600,
            },
            &limits,
        )
        .expect_err("an over-budget intermediate must be refused");
        assert!(matches!(
            error,
            SvcError::BadRequest("resize would exceed the memory budget")
        ));
    }

    #[test]
    fn parse_rest_rejects_zero_quality() {
        // `ravif::Encoder::with_quality` asserts `1.0..=100.0`.
        let err = parse_rest("f:avif/q:0/rs:fit:10:10/plain/https://e.com/a.png").unwrap_err();
        assert!(matches!(err, SvcError::BadRequest("bad quality")));
    }

    #[test]
    fn resize_validate_rejects_a_target_over_the_cap_before_any_fetch() {
        let resize = Resize {
            mode: ResizeMode::Fit,
            w: 60_000,
            h: 10,
        };
        assert!(matches!(
            resize.validate(16_384),
            Err(SvcError::BadRequest("resize target too large"))
        ));
        assert!(resize.validate(65_535).is_ok());
    }

    #[test]
    fn encode_image_encodes_a_grayscale_source_as_webp() {
        // `webp::Encoder::from_image` returns "Unimplemented" for Luma8, so every
        // grayscale PNG used to 500 on the Blossom default output format.
        let gray =
            DynamicImage::ImageLuma8(image::GrayImage::from_pixel(16, 16, image::Luma([128])));
        let out = encode_image(&gray, &OutFmt::Webp, 80).expect("grayscale must encode");
        assert_eq!(&out[..4], b"RIFF");
        assert_eq!(&out[8..12], b"WEBP");
    }

    #[test]
    fn encode_image_rejects_a_webp_target_over_the_libwebp_cap() {
        let error = encode_image(&img(16_384, 1), &OutFmt::Webp, 80)
            .expect_err("libwebp cannot encode a 16384-wide image");
        assert!(matches!(
            error,
            SvcError::BadRequest("output size unsupported by webp")
        ));
    }

    // -----------------------------------------------------------------------
    // decode_image — input format allowlist
    // -----------------------------------------------------------------------

    #[test]
    fn decode_image_rejects_gif_as_unsupported_source() {
        // Minimal 1x1 GIF89a: the magic bytes are enough for the content
        // sniff, but GIF is not on the input allowlist. Two rejections are
        // permissible — the allowlist firing (`BadRequest`, the intended
        // mechanism) or the decoder itself refusing (a build with no GIF
        // decoder) — and the arms below distinguish them. A successful decode
        // is the contract violation the test pins: "no decode".
        let gif: &[u8] =
            b"GIF89a\x01\x00\x01\x00\x00\x00\x00\x2c\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02\x44\x01\x00\x3b";
        match decode_image(gif, test_limits()) {
            Err(SvcError::BadRequest("unsupported source format")) => {} // allowlist fired
            Err(SvcError::Decode(_)) => {} // decoder refused, still no decode
            Err(other) => panic!("unexpected rejection: {other:?}"),
            Ok(_) => panic!("a GIF was decoded: input format allowlist not enforced"),
        }
    }

    #[test]
    fn decode_image_rejects_avif_as_unsupported_source() {
        // ISO-BMFF `ftyp` box with the "avif" brand: sniffed as AVIF, which is
        // an output-only format since `avif-native` was dropped. The same
        // allowlist rule applies — the behaviour change this pins is that
        // `/thumb` on an AVIF blob now yields 400 instead of a thumbnail.
        let avif: &[u8] = b"\x00\x00\x00\x18ftypavif\x00\x00\x00\x00avifmif1";
        match decode_image(avif, test_limits()) {
            Err(SvcError::BadRequest("unsupported source format")) => {} // allowlist fired
            Err(SvcError::Decode(_)) => {} // decoder refused, still no decode
            Err(other) => panic!("unexpected rejection: {other:?}"),
            Ok(_) => panic!("an AVIF was decoded: input format allowlist not enforced"),
        }
    }
}
