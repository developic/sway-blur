//! Screen capture via `zwlr_screencopy_v1`: whole output into SHM, crop in
//! software. Output picked by `zxdg_output_v1` name, geometry fallback.

use crate::model::{OutRect, Rect};
use anyhow::{bail, Context, Result};
use image::RgbaImage;
use std::cell::RefCell;
use std::os::unix::io::AsFd;
use std::time::{Duration, Instant};
use wayland_client::{
    protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool},
    Connection, Dispatch, QueueHandle, WEnum,
};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1, zxdg_output_v1,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

const ROUNDTRIP_TIMEOUT: Duration = Duration::from_secs(10);

/// One output: Wayland proxy + xdg-output name/geometry.
struct Output {
    proxy: wl_output::WlOutput,
    name: Option<String>,
    lx: i32,
    ly: i32,
    lw: i32,
    lh: i32,
}

#[derive(Default)]
struct FrameState {
    format: Option<wl_shm::Format>,
    width: u32,
    height: u32,
    stride: u32,
    have_buffer: bool,
    ready: bool,
    failed: bool,
}

struct State {
    shm: Option<wl_shm::WlShm>,
    screencopy: Option<ZwlrScreencopyManagerV1>,
    xdg_manager: Option<ZxdgOutputManagerV1>,
    outputs: Vec<Output>,
    frame: FrameState,
}

// --- registry ---------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_shm" => {
                    state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(name, 1, qh, ()));
                }
                "wl_output" => {
                    let proxy = registry.bind::<wl_output::WlOutput, _, _>(name, 1, qh, ());
                    state.outputs.push(Output {
                        proxy,
                        name: None,
                        lx: 0,
                        ly: 0,
                        lw: 0,
                        lh: 0,
                    });
                }
                // Bind v1 only: no dmabuf/damage negotiation.
                "zwlr_screencopy_manager_v1" => {
                    state.screencopy =
                        Some(registry.bind::<ZwlrScreencopyManagerV1, _, _>(name, 1, qh, ()));
                }
                "zxdg_output_manager_v1" => {
                    state.xdg_manager = Some(registry.bind::<ZxdgOutputManagerV1, _, _>(
                        name,
                        version.min(3),
                        qh,
                        (),
                    ));
                }
                _ => {}
            }
        }
    }
}

// --- no-op object events (we ignore output geometry, shm formats, release) ---

wayland_client::delegate_noop!(State: ignore wl_output::WlOutput);
wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(State: ignore ZxdgOutputManagerV1);
wayland_client::delegate_noop!(State: ignore ZwlrScreencopyManagerV1);

// --- xdg-output per-output info (userdata = index into outputs) -------

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(info) = state.outputs.get_mut(*idx) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::Name { name } => info.name = Some(name),
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                info.lx = x;
                info.ly = y;
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                info.lw = width;
                info.lh = height;
            }
            _ => {}
        }
    }
}

// --- screencopy frame negotiation -------------------------------------

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                let WEnum::Value(format) = format else {
                    state.frame.failed = true;
                    return;
                };
                state.frame.format = Some(format);
                state.frame.width = width;
                state.frame.height = height;
                state.frame.stride = stride;
                state.frame.have_buffer = true;
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                state.frame.ready = true;
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                state.frame.failed = true;
            }
            _ => {}
        }
    }
}

// --- capture (persistent Capturer: connect once, then frame negotiation + copy per shot) ---

/// Owns the Wayland connection + globals, reused every shot.
/// Not `Send`: lives on its creator thread (the daemon worker).
pub struct Capturer {
    queue: wayland_client::EventQueue<State>,
    state: State,
}

impl Capturer {
    /// Connect, bind globals, resolve output names.
    pub fn new() -> Result<Self> {
        let conn = Connection::connect_to_env()
            .context("connecting to Wayland (WAYLAND_DISPLAY not set?)")?;
        let mut queue: wayland_client::EventQueue<State> = conn.new_event_queue();
        let qh = queue.handle();
        let display = conn.display();
        let _registry = display.get_registry(&qh, ());
        let mut state = State {
            shm: None,
            screencopy: None,
            xdg_manager: None,
            outputs: Vec::new(),
            frame: FrameState::default(),
        };
        queue
            .roundtrip(&mut state)
            .context("Wayland roundtrip (globals)")?;
        if state.shm.is_none() {
            bail!("compositor offers no wl_shm");
        }
        if state.screencopy.is_none() {
            bail!("compositor offers no zwlr_screencopy_manager_v1 (not wlroots?)");
        }
        if state.outputs.is_empty() {
            bail!("compositor exposes no wl_output globals");
        }
        // Resolve names + logical geometry via xdg-output.
        if let Some(xdg) = state.xdg_manager.clone() {
            for (i, o) in state.outputs.iter().enumerate() {
                let _ = xdg.get_xdg_output(&o.proxy, &qh, i);
            }
            queue
                .roundtrip(&mut state)
                .context("Wayland roundtrip (xdg-output)")?;
            queue
                .roundtrip(&mut state)
                .context("Wayland roundtrip (xdg-output done)")?;
        }
        Ok(Self { queue, state })
    }

    /// Wait for frame state with a deadline poll. Never hangs the worker past it.
    fn wait_frame(
        &mut self,
        deadline: Instant,
        ctx: &'static str,
        mut done: impl FnMut(&FrameState) -> bool,
    ) -> Result<()> {
        use rustix::event::{PollFd, PollFlags, Timespec};
        // Flush first: requests sit in the send buffer until flushed.
        self.queue.flush().context("flushing Wayland requests")?;
        loop {
            self.queue
                .dispatch_pending(&mut self.state)
                .context("Wayland dispatch")?;
            if done(&self.state.frame) {
                return Ok(());
            }
            if self.state.frame.failed {
                bail!("screencopy frame failed ({ctx})");
            }
            // Zero budget: poll once, then bail below on deadline.
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("screencopy: no {ctx} within 10s");
            }
            // `dispatch_pending` never reads the socket: poll for readability first.
            let budget = Timespec::try_from(remaining).unwrap_or(Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            });
            let fd = self.queue.as_fd();
            let mut fds = [PollFd::new(&fd, PollFlags::IN)];
            rustix::event::poll(&mut fds, Some(&budget)).context("polling Wayland socket")?;
            let rev = fds[0].revents();
            if rev.contains(PollFlags::IN) {
                // Events may already have arrived: then nothing to read.
                if let Some(guard) = self.queue.prepare_read() {
                    guard.read().context("Wayland socket read")?;
                }
            } else if rev.intersects(PollFlags::HUP | PollFlags::ERR) {
                bail!("Wayland connection lost");
            }
            // else: poll timed out; the loop re-checks the deadline.
        }
    }

    /// Capture an output-local `rect`: whole output via wlr-screencopy, cropped
    /// in software. `fallback` (GET_OUTPUTS geometry) picks the `wl_output`
    /// when xdg-output names are unavailable.
    pub fn capture(
        &mut self,
        output_name: &str,
        rect: &Rect,
        fallback: &OutRect,
    ) -> Result<RgbaImage> {
        let deadline = Instant::now() + ROUNDTRIP_TIMEOUT;
        // Drain stale events (e.g. wl_buffer Release from the last shot).
        self.queue
            .dispatch_pending(&mut self.state)
            .context("Wayland dispatch")?;

        // Exact name, then geometry, then first. Unknown output on multi-output
        // means hotplug: bail so the caller rebuilds the Capturer.
        let name_hit = self
            .state
            .outputs
            .iter()
            .position(|o| o.name.as_deref() == Some(output_name));
        let geom_hit = self.state.outputs.iter().position(|o| {
            o.lw > 0
                && o.lx == fallback.x
                && o.ly == fallback.y
                && o.lw == fallback.width
                && o.lh == fallback.height
        });
        if self.state.outputs.is_empty() {
            bail!("compositor exposes no wl_output globals");
        }
        if self.state.outputs.len() > 1 && name_hit.is_none() && geom_hit.is_none() {
            bail!("output {output_name} unknown (hotplug?); rebuilding capturer");
        }
        let idx = name_hit.or(geom_hit).unwrap_or(0);
        let logical_w = if self.state.outputs[idx].lw > 0 {
            self.state.outputs[idx].lw
        } else {
            fallback.width.max(1)
        };

        let shm = self.state.shm.clone().context("wl_shm lost")?;
        let manager = self
            .state
            .screencopy
            .clone()
            .context("screencopy manager lost")?;
        let target = self.state.outputs[idx].proxy.clone();
        let qh = self.queue.handle();

        // Fresh per-shot frame state (the queue may hold stale events).
        self.state.frame = FrameState::default();
        // Capture the whole output (cursor excluded), then crop.
        let frame = manager.capture_output(0, &target, &qh, ());
        self.wait_frame(deadline, "buffer parameters", |f| f.have_buffer)?;
        let (format, fw, fh, stride) = (
            self.state
                .frame
                .format
                .context("screencopy offered no format")?,
            self.state.frame.width,
            self.state.frame.height,
            self.state.frame.stride,
        );
        if !matches!(
            format,
            wl_shm::Format::Argb8888
                | wl_shm::Format::Xrgb8888
                | wl_shm::Format::Abgr8888
                | wl_shm::Format::Xbgr8888
        ) {
            bail!("screencopy: unsupported buffer format {format:?}");
        }
        if fw < 2 || fh < 2 || stride < fw * 4 {
            bail!("screencopy: bogus buffer geometry {fw}x{fh} stride {stride}");
        }

        // SHM backing must live on tmpfs: XDG_RUNTIME_DIR or bail.
        let size = stride as usize * fh as usize;
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(std::path::PathBuf::from)
            .context("XDG_RUNTIME_DIR not set; cannot create screencopy shm backing")?;
        let path = dir.join(format!("sway-blur-{}-{idx}", std::process::id()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format!("creating shm backing {}", path.display()))?;
        file.set_len(size as u64).context("sizing shm backing")?;
        // SAFETY: file is a fresh mapping we own; the compositor writes frame
        // pixels into it exactly once before signalling Ready.
        let mmap = unsafe { memmap2::MmapMut::map_mut(&file)? };
        let _ = std::fs::remove_file(&path);

        let pool = shm.create_pool(file.as_fd(), size as i32, &qh, ());
        let buffer = pool.create_buffer(0, fw as i32, fh as i32, stride as i32, format, &qh, ());
        pool.destroy();
        frame.copy(&buffer);

        self.wait_frame(deadline, "frame copy", |f| f.ready)?;

        // Convert wire pixels (native-endian xRGB) to RGBA.
        let is_argb = format == wl_shm::Format::Argb8888;
        let is_abgr = format == wl_shm::Format::Abgr8888;
        let mut rgba = vec![0u8; fw as usize * fh as usize * 4];
        for y in 0..fh as usize {
            let src_row = &mmap[y * stride as usize..][..fw as usize * 4];
            let dst_row = &mut rgba[y * fw as usize * 4..][..fw as usize * 4];
            let (src_px, _) = src_row.as_chunks::<4>();
            let (dst_px, _) = dst_row.as_chunks_mut::<4>();
            for (s, d) in src_px.iter().zip(dst_px.iter_mut()) {
                if is_argb || format == wl_shm::Format::Xrgb8888 {
                    d[0] = s[2];
                    d[1] = s[1];
                    d[2] = s[0];
                    d[3] = if is_argb { s[3] } else { 255 };
                } else {
                    d[0] = s[0];
                    d[1] = s[1];
                    d[2] = s[2];
                    d[3] = if is_abgr { s[3] } else { 255 };
                }
            }
        }
        buffer.destroy();
        frame.destroy();
        drop(mmap);

        let full = RgbaImage::from_raw(fw, fh, rgba).context("assembling captured frame")?;

        // Rect is in sway logical pixels; the buffer may be scaled up.
        let scale = fw as f64 / logical_w as f64;
        let cx = ((rect.x as f64 * scale).round() as i32).clamp(0, fw as i32 - 1);
        let cy = ((rect.y as f64 * scale).round() as i32).clamp(0, fh as i32 - 1);
        let cw = ((rect.width as f64 * scale).round() as i32).clamp(1, fw as i32 - cx);
        let ch = ((rect.height as f64 * scale).round() as i32).clamp(1, fh as i32 - cy);
        Ok(image::imageops::crop_imm(&full, cx as u32, cy as u32, cw as u32, ch as u32).to_image())
    }
}

// --- shared session (one persistent Capturer per thread, rebuilt on any error) ---

thread_local! {
    static CAPTURER: RefCell<Option<Capturer>> = const { RefCell::new(None) };
}

/// Capture via the thread's persistent connection (no per-shot roundtrips).
pub fn capture_rect(output_name: &str, rect: &Rect, fallback: &OutRect) -> Result<RgbaImage> {
    CAPTURER.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(Capturer::new()?);
        }
        let r = slot
            .as_mut()
            .expect("capturer just created")
            .capture(output_name, rect, fallback);
        if r.is_err() {
            // Drop the session so the next shot starts fresh.
            *slot = None;
        }
        r
    })
}
