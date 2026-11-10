use crate::model::{OutRect, Pixels, Rect, SurfaceKey};
use gdk::prelude::*;
use gtk::prelude::*;
use gtk_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::collections::{HashMap, HashSet};

struct Entry {
    win: gtk::Window,
    img: gtk::Image,
}

/// Bottom-layer surfaces showing blurred snapshots. Lives on the GTK thread;
/// workers drive it through [`OverlayHandle`] messages.
struct Overlay {
    windows: HashMap<SurfaceKey, Entry>,
}

impl Overlay {
    pub fn new() -> Self {
        Self {
            windows: HashMap::new(),
        }
    }

    /// Sway output -> GDK monitor by geometry; falls back to primary.
    fn find_monitor(&self, out_rect: &OutRect) -> Option<gdk::Monitor> {
        let display = gdk::Display::default()?;
        let n = display.n_monitors();
        for i in 0..n {
            if let Some(mon) = display.monitor(i) {
                let g = mon.geometry();
                if g.x() == out_rect.x
                    && g.y() == out_rect.y
                    && g.width() == out_rect.width
                    && g.height() == out_rect.height
                {
                    return Some(mon);
                }
            }
        }
        for i in 0..n {
            if let Some(mon) = display.monitor(i) {
                if mon.is_primary() {
                    return Some(mon);
                }
            }
        }
        if n > 0 {
            display.monitor(0)
        } else {
            None
        }
    }

    fn get_or_create_window(&mut self, key: &SurfaceKey, out_rect: &OutRect) -> &mut Entry {
        if !self.windows.contains_key(key) {
            let win = gtk::Window::new(gtk::WindowType::Toplevel);
            win.set_decorated(false);
            win.set_skip_taskbar_hint(true);
            win.set_skip_pager_hint(true);
            win.set_accept_focus(false);
            win.set_focus_on_map(false);
            let img = gtk::Image::new();
            win.add(&img);
            win.init_layer_shell();
            win.set_layer(Layer::Background);
            win.set_anchor(Edge::Top, true);
            win.set_anchor(Edge::Left, true);
            win.set_anchor(Edge::Right, false);
            win.set_anchor(Edge::Bottom, false);
            win.set_exclusive_zone(-1);
            // keyboard: never take input
            win.set_keyboard_mode(KeyboardMode::None);
            win.set_namespace("sway-blur");
            if let Some(mon) = self.find_monitor(out_rect) {
                win.set_monitor(&mon);
            }
            self.windows.insert(key.clone(), Entry { win, img });
        }
        self.windows.get_mut(key).expect("window was just inserted")
    }

    // -- GTK-thread-only primitives -------------------------------------

    fn apply(&mut self, msg: OverlayMsg) {
        match msg {
            OverlayMsg::Sync(frames) => self.sync(frames),
            OverlayMsg::HideAll => self.hide_all(),
        }
    }

    /// Show or hide one surface. `pixels: None` keeps the surface alive (for a
    /// retry) but hidden; it never displays a stale frame.
    fn show(&mut self, frame: Frame) {
        let Frame {
            key,
            pixels,
            width,
            height,
            rect,
            out_rect,
        } = frame;
        let entry = self.get_or_create_window(&key, &out_rect);
        entry.win.set_layer_shell_margin(Edge::Top, rect.y);
        entry.win.set_layer_shell_margin(Edge::Left, rect.x);
        entry.win.set_size_request(rect.width, rect.height);
        match pixels {
            Some(pixels) => {
                // from_bytes keeps a ref to Bytes; Bytes owns the shared buffer.
                let bytes = glib::Bytes::from_owned(pixels);
                let pixbuf = gdk_pixbuf::Pixbuf::from_bytes(
                    &bytes,
                    gdk_pixbuf::Colorspace::Rgb,
                    true,
                    8,
                    width,
                    height,
                    width * 4,
                );
                entry.img.set_from_pixbuf(Some(&pixbuf));
                entry.win.show_all();
            }
            None => entry.win.hide(),
        }
    }

    fn hide_all(&mut self) {
        for entry in self.windows.values() {
            entry.win.hide();
        }
    }

    /// Apply a full desired frame set atomically: show/update everything, then
    /// destroy surfaces that are no longer live.
    fn sync(&mut self, frames: Vec<Frame>) {
        let live: HashSet<SurfaceKey> = frames.iter().map(|f| f.key.clone()).collect();
        for frame in frames {
            self.show(frame);
        }
        self.prune(&live);
    }

    fn prune(&mut self, live: &HashSet<SurfaceKey>) {
        let dead: Vec<SurfaceKey> = self
            .windows
            .keys()
            .filter(|k| !live.contains(*k))
            .cloned()
            .collect();
        for k in dead {
            if let Some(entry) = self.windows.remove(&k) {
                // SAFETY: runs on the GTK thread.
                unsafe { entry.win.destroy() };
            }
        }
    }
}

// -- cross-thread handle (lock-free channel; sends after shutdown fail silently) --

/// One terminal's finished frame, posted to the GTK thread.
pub struct Frame {
    pub key: SurfaceKey,
    pub pixels: Option<Pixels>,
    pub width: i32,
    pub height: i32,
    pub rect: Rect,
    pub out_rect: OutRect,
}

/// Work the GTK thread performs for the daemon worker.
pub enum OverlayMsg {
    HideAll,
    Sync(Vec<Frame>),
}

/// Cloneable fire-and-forget handle the worker uses to drive the overlay.
#[derive(Clone)]
pub struct OverlayHandle {
    tx: async_channel::Sender<OverlayMsg>,
}

impl OverlayHandle {
    /// Build on the GTK thread (after `gtk::init`, before `gtk::main`).
    pub fn spawn() -> Self {
        let (tx, rx) = async_channel::unbounded();
        let mut overlay = Overlay::new();
        glib::MainContext::default().spawn_local(async move {
            while let Ok(msg) = rx.recv().await {
                overlay.apply(msg);
            }
        });
        Self { tx }
    }

    /// Apply the complete desired set of surfaces in one GTK-thread turn.
    pub fn sync(&self, frames: Vec<Frame>) {
        let _ = self.tx.try_send(OverlayMsg::Sync(frames));
    }

    pub fn hide_all(&self) {
        let _ = self.tx.try_send(OverlayMsg::HideAll);
    }
}
