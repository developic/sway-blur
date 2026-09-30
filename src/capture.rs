use crate::config::Config;
use crate::ipc;
use crate::model::{OutRect, Rect};
use crate::screencopy;
use fast_image_resize as fir;
use image::RgbaImage;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct CaptureError(pub String);

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for CaptureError {}

/// Screenshot an output-local rect via the wlr-screencopy Wayland protocol.
/// Returns raw (unblurred) RGBA pixels, cropped to `rect`.
pub fn capture_region(
    rect: &Rect,
    output: &str,
    out_rect: &OutRect,
) -> Result<RgbaImage, CaptureError> {
    screencopy::capture_rect(output, rect, out_rect)
        .map_err(|e| CaptureError(format!("screencopy failed on {}: {e:#}", rect.geometry())))
}

/// SIMD Bilinear RGBA resize. Blurred right after/before, so it matches
/// Triangle at a fraction of the cost.
fn resize_rgba(src: &RgbaImage, dw: u32, dh: u32) -> RgbaImage {
    let src_img = fir::images::Image::from_vec_u8(
        src.width(),
        src.height(),
        src.as_raw().to_vec(),
        fir::PixelType::U8x4,
    )
    .expect("src dimensions are valid");
    let mut dst_img = fir::images::Image::new(dw, dh, fir::PixelType::U8x4);
    let mut resizer = fir::Resizer::new();
    let opts = fir::ResizeOptions::new()
        .resize_alg(fir::ResizeAlg::Convolution(fir::FilterType::Bilinear));
    resizer
        .resize(&src_img, &mut dst_img, &opts)
        .expect("dst dimensions are valid");
    RgbaImage::from_raw(dw, dh, dst_img.into_vec()).expect("fir output size matches")
}

/// Blur, pure Rust: downscale -> gaussian blur -> upscale -> optional
/// dark tint. Works on raw pixels, returns raw pixels.
pub fn blur_image(cropped: &RgbaImage, config: &Config) -> RgbaImage {
    let (w, h) = (cropped.width(), cropped.height());
    let dw = (w / config.downscale.max(1)).max(1);
    let dh = (h / config.downscale.max(1)).max(1);
    let small = resize_rgba(cropped, dw, dh);
    let blurred: RgbaImage = image::imageops::fast_blur(&small, config.blur as f32);
    let mut big = resize_rgba(&blurred, w, h);

    // Optional dark tint (keep = 1.0 means none).
    let keep = ((100.0 - config.tint) / 100.0) as f32;
    if keep < 1.0 {
        for px in big.pixels_mut() {
            for i in 0..3 {
                px[i] = (px[i] as f32 * keep).clamp(0.0, 255.0) as u8;
            }
        }
    }

    big
}

/// Capture the pixels BEHIND a window: hide it (opacity 0), screencopy,
/// restore in all cases. Returns (rgba_pixels, invisible_ms).
pub fn capture_window(
    rect: &Rect,
    con_id: i64,
    config: &Config,
    output: &str,
    out_rect: &OutRect,
) -> Result<(RgbaImage, f64), CaptureError> {
    let reply = ipc::once_reused(ipc::T_RUN_COMMAND, &format!("[con_id={con_id}] opacity 0"))
        .map_err(|e| CaptureError(format!("opacity 0 IPC failed: {e:#}")))?;
    let ok = reply
        .as_array()
        .map(|arr| {
            arr.iter()
                .all(|r| r.get("success").and_then(|s| s.as_bool()).unwrap_or(false))
        })
        .unwrap_or(false);
    if !ok {
        return Err(CaptureError(format!("opacity 0 rejected: {reply}")));
    }
    let start = Instant::now();
    std::thread::sleep(Duration::from_secs_f64(config.hide_settle_s));
    let raw = capture_region(rect, output, out_rect);
    // finally: restore opacity no matter what capture did.
    match ipc::once_reused(
        ipc::T_RUN_COMMAND,
        &format!("[con_id={con_id}] opacity 1.0"),
    ) {
        Ok(_) => {}
        Err(e) => eprintln!("[capture] WARNING: opacity restore failed for {con_id}: {e:#}"),
    }
    let invisible_ms = start.elapsed().as_secs_f64() * 1000.0;
    Ok((raw?, invisible_ms))
}

/// Hide the window, capture behind-pixels, blur+tint.
pub fn capture_and_blur(
    rect: &Rect,
    config: &Config,
    con_id: i64,
    output: &str,
    out_rect: &OutRect,
) -> Result<RgbaImage, CaptureError> {
    let start = Instant::now();
    let (raw, invisible_ms) = capture_window(rect, con_id, config, output, out_rect)?;
    let cap_ms = start.elapsed().as_secs_f64() * 1000.0;
    let out = blur_image(&raw, config);
    let total_ms = start.elapsed().as_secs_f64() * 1000.0;
    println!(
        "[capture] screencopy {:.0}ms (window hidden {:.0}ms) + blur {:.0}ms = {:.0}ms for {}x{}",
        cap_ms,
        invisible_ms,
        total_ms - cap_ms,
        total_ms,
        out.width(),
        out.height()
    );
    let _ = std::io::Write::flush(&mut std::io::stdout());
    Ok(out)
}
