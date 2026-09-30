use crate::model::{OutRect, Rect};
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
    windows: HashMap<(String, i64), Entry>,
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

    fn get_or_create_window(&mut self, key: &(String, i64), out_rect: &OutRect) -> &Entry {
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
        &self.windows[key]
    }

    // -- GTK-thread-only primitives -------------------------------------

    fn apply(&mut self, msg: OverlayMsg) {
        match msg {
            OverlayMsg::Show {
                pixels,
                width,
                height,
                key,
                rect,
                out_rect,
            } => self.show(&pixels, width, height, key, &rect, &out_rect),
            OverlayMsg::HideAll => self.hide_all(),
            OverlayMsg::Prune { live } => self.prune(&live),
        }
    }

    /// Show raw RGBA pixels at rect (output-local); one surface per (output, con_id).
    fn show(
        &mut self,
        pixels: &[u8],
        width: i32,
        height: i32,
        key: (String, i64),
        rect: &Rect,
        out_rect: &OutRect,
    ) {
        // Clone key fields before the mutable borrow below.
        let entry = self.get_or_create_window(&key, out_rect);
        let win = entry.win.clone();
        let img = entry.img.clone();
        win.set_layer_shell_margin(Edge::Top, rect.y);
        win.set_layer_shell_margin(Edge::Left, rect.x);
        win.set_size_request(rect.width, rect.height);
        // from_bytes references (not copies) the bytes; they live until replaced.
        let bytes = glib::Bytes::from(pixels);
        let pixbuf = gdk_pixbuf::Pixbuf::from_bytes(
            &bytes,
            gdk_pixbuf::Colorspace::Rgb,
            true,
            8,
            width,
            height,
            width * 4,
        );
        img.set_from_pixbuf(Some(&pixbuf));
        win.show_all();
    }

    fn hide_all(&mut self) {
        for entry in self.windows.values() {
            entry.win.hide();
        }
    }

    /// Destroy overlays for windows that are no longer visible.
    fn prune(&mut self, live_keys: &HashSet<(String, i64)>) {
        let dead: Vec<(String, i64)> = self
            .windows
            .keys()
            .filter(|k| !live_keys.contains(*k))
            .cloned()
            .collect();
        for k in dead {
            if let Some(entry) = self.windows.remove(&k) {
                // SAFETY: called on the GTK thread via idle_add.
                unsafe { entry.win.destroy() };
            }
        }
    }
}

// -- cross-thread handle (lock-free channel; sends after shutdown fail silently) --

/// Work the GTK thread performs for the daemon worker.
pub enum OverlayMsg {
    Show {
        pixels: Vec<u8>,
        width: i32,
        height: i32,
        key: (String, i64),
        rect: Rect,
        out_rect: OutRect,
    },
    HideAll,
    Prune {
        live: HashSet<(String, i64)>,
    },
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

    pub fn show(
        &self,
        pixels: Vec<u8>,
        width: i32,
        height: i32,
        key: (String, i64),
        rect: Rect,
        out_rect: OutRect,
    ) {
        let _ = self.tx.try_send(OverlayMsg::Show {
            pixels,
            width,
            height,
            key,
            rect,
            out_rect,
        });
    }

    pub fn hide_all(&self) {
        let _ = self.tx.try_send(OverlayMsg::HideAll);
    }

    pub fn prune(&self, live: HashSet<(String, i64)>) {
        let _ = self.tx.try_send(OverlayMsg::Prune { live });
    }
}
