use crate::config::Config;
use crate::ipc;
use crate::model::{OutRect, Rect};
use crate::screencopy;
use fast_image_resize as fir;
use image::RgbaImage;
use std::cell::RefCell;
use std::time::{Duration, Instant};

// Reused across resizes on this thread: keeps convolution buffers alive
// instead of reallocating them twice per capture.
thread_local! {
    static RESIZER: RefCell<fir::Resizer> = RefCell::new(fir::Resizer::new());
}

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
/// Triangle at a fraction of the cost. Zero-copy on the source side:
/// `RgbaImage` is viewed directly (no `to_vec` clone); only the
/// destination allocates, which is unavoidable.
fn resize_rgba(src: &RgbaImage, dw: u32, dh: u32) -> RgbaImage {
    let mut dst = RgbaImage::new(dw, dh);
    let opts = fir::ResizeOptions::new()
        .resize_alg(fir::ResizeAlg::Convolution(fir::FilterType::Bilinear));
    RESIZER.with(|r| {
        r.borrow_mut()
            .resize(src, &mut dst, &opts)
            .expect("dst dimensions are valid")
    });
    dst
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

    // Optional dark tint (keep = 1.0 means none). LUT: one float
    // mult per level instead of per channel per pixel.
    let keep = ((100.0 - config.tint) / 100.0) as f32;
    if keep < 1.0 {
        let lut: [u8; 256] = std::array::from_fn(|i| (i as f32 * keep).clamp(0.0, 255.0) as u8);
        for px in big.pixels_mut() {
            px[0] = lut[px[0] as usize];
            px[1] = lut[px[1] as usize];
            px[2] = lut[px[2] as usize];
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
