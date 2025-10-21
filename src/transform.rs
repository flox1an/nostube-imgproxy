use image::{imageops::FilterType, DynamicImage, GenericImageView};
use percent_encoding::percent_decode_str;

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
}

#[derive(Debug, Clone)]
pub struct Resize {
    pub mode: ResizeMode,
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Clone)]
pub enum ResizeMode {
    Fit,
    Fill,
    FillDown,
    Force,
    Auto,
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
                .filter(|q: &u8| *q <= 100)
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
fn parse_resize_directive(arg: &str) -> Result<Resize, SvcError> {
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

/// Apply resize transformation based on the resize mode
pub fn apply_resize(img: DynamicImage, resize: &Resize) -> DynamicImage {
    let (src_w, src_h) = img.dimensions();
    
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
        ResizeMode::Fit => apply_resize_fit(img, target_w, target_h),
        ResizeMode::Fill => apply_resize_fill(img, target_w, target_h),
        ResizeMode::FillDown => apply_resize_fill_down(img, target_w, target_h),
        ResizeMode::Force => apply_resize_force(img, target_w, target_h),
        ResizeMode::Auto => unreachable!(), // Already resolved above
    }
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
fn apply_resize_fit(img: DynamicImage, target_w: u32, target_h: u32) -> DynamicImage {
    let (w, h) = img.dimensions();

    // Scale to fit within the box
    let scale = f32::min(target_w as f32 / w as f32, target_h as f32 / h as f32);
    
    // Don't upscale if image is smaller
    let scale = f32::min(scale, 1.0);
    
    let new_w = (w as f32 * scale).round() as u32;
    let new_h = (h as f32 * scale).round() as u32;

    img.resize_exact(new_w, new_h, FilterType::Lanczos3)
}

/// Fill: Resize while keeping aspect ratio to fill the given size, with center crop
fn apply_resize_fill(img: DynamicImage, target_w: u32, target_h: u32) -> DynamicImage {
    let (w, h) = img.dimensions();

    // Scale to fill the box
    let scale = f32::max(target_w as f32 / w as f32, target_h as f32 / h as f32);
    let new_w = (w as f32 * scale).ceil() as u32;
    let new_h = (h as f32 * scale).ceil() as u32;

    let resized = img.resize_exact(new_w, new_h, FilterType::Lanczos3);

    // Center crop
    let x = (new_w.saturating_sub(target_w)) / 2;
    let y = (new_h.saturating_sub(target_h)) / 2;
    resized.crop_imm(x, y, target_w, target_h)
}

/// Fill-Down: Like fill, but if result is smaller, crop to maintain aspect ratio
fn apply_resize_fill_down(img: DynamicImage, target_w: u32, target_h: u32) -> DynamicImage {
    let (w, h) = img.dimensions();

    // Scale to fill the box
    let scale = f32::max(target_w as f32 / w as f32, target_h as f32 / h as f32);
    
    // Don't upscale
    let scale = f32::min(scale, 1.0);
    
    let new_w = (w as f32 * scale).ceil() as u32;
    let new_h = (h as f32 * scale).ceil() as u32;

    let resized = img.resize_exact(new_w, new_h, FilterType::Lanczos3);

    // If smaller than target, crop to maintain aspect ratio
    let crop_w = new_w.min(target_w);
    let crop_h = new_h.min(target_h);
    
    // Center crop
    let x = (new_w.saturating_sub(crop_w)) / 2;
    let y = (new_h.saturating_sub(crop_h)) / 2;
    resized.crop_imm(x, y, crop_w, crop_h)
}

/// Force: Resize without keeping aspect ratio
fn apply_resize_force(img: DynamicImage, target_w: u32, target_h: u32) -> DynamicImage {
    img.resize_exact(target_w, target_h, FilterType::Lanczos3)
}

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
            // Use lossy WebP encoding with quality control
            let webp_data = webp::Encoder::from_image(img)
                .map_err(|e| SvcError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?
                .encode(quality as f32);
            out.extend_from_slice(&webp_data);
        }
        OutFmt::Avif => {
            // Use ravif for AVIF encoding with quality control
            let rgba = img.to_rgba8();
            let (w, h) = (rgba.width(), rgba.height());

            // Convert to rgb::RGBA format
            let pixels: Vec<rgb::RGBA<u8>> = rgba
                .pixels()
                .map(|p| rgb::RGBA {
                    r: p[0],
                    g: p[1],
                    b: p[2],
                    a: p[3],
                })
                .collect();

            let avif_img = ravif::Img::new(&pixels[..], w as usize, h as usize);
            let encoder = ravif::Encoder::new()
                .with_quality(quality as f32)
                .with_speed(6);
            let encoded = encoder.encode_rgba(avif_img).map_err(|e| {
                SvcError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("AVIF encode error: {}", e),
                ))
            })?;
            out.extend_from_slice(&encoded.avif_file);
        }
    }
    Ok(out)
}

