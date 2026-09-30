use crate::capture::{self, CaptureError};
use crate::config::Config;
use crate::ipc;
use crate::model::{OutRect, Rect, Snapshot, Term};
use crate::overlay::OverlayHandle;
use crate::tree;
use crate::watcher::Watcher;
use anyhow::Result;
use image::RgbaImage;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Condvar, Mutex, MutexGuard,
};
use std::thread;
use std::time::{Duration, Instant};

/// Poison-tolerant lock: a panicked thread must not wedge the daemon.
/// State here is flags/caches — safe to keep using after a poison.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn flush() {
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

struct Cache {
    map: HashMap<i64, Snapshot>,
    order: VecDeque<i64>,
}

impl Cache {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Hit clones pixels and refreshes recency; stale rect/output/below misses.
    fn get_for(&mut self, t: &Term) -> Option<RgbaImage> {
        let hit = self.map.get(&t.con_id)?;
        if hit.output != t.output || hit.rect != t.rect || hit.below != t.below {
            return None;
        }
        let img = hit.img.clone();
        if let Some(pos) = self.order.iter().position(|&id| id == t.con_id) {
            self.order.remove(pos);
            self.order.push_back(t.con_id);
        }
        Some(img)
    }

    fn put(&mut self, t: &Term, img: RgbaImage, max: usize) {
        if !self.map.contains_key(&t.con_id) {
            self.order.push_back(t.con_id);
        }
        self.map.insert(
            t.con_id,
            Snapshot {
                img,
                rect: t.rect,
                output: t.output.clone(),
                below: t.below.clone(),
            },
        );
        if self.map.len() > max {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }
}

struct Shared {
    config: Config,
    outputs: Mutex<HashMap<String, OutRect>>,
    cache: Mutex<Cache>,
    live_keys: Mutex<HashSet<(String, i64)>>,
    dirty: Mutex<bool>,
    cvar: Condvar,
    stop: AtomicBool,
    /// Lock-free message handle to the GTK thread.
    overlay: OverlayHandle,
}

/// One terminal's finished frame, posted to the GTK thread.
struct PendingShow {
    key: (String, i64),
    pixels: Vec<u8>,
    width: i32,
    height: i32,
    rect: Rect,
    out_rect: OutRect,
}

impl PendingShow {
    fn of(key: (String, i64), img: &RgbaImage, rect: Rect, out_rect: OutRect) -> Self {
        Self {
            key,
            pixels: img.clone().into_raw(),
            width: img.width() as i32,
            height: img.height() as i32,
            rect,
            out_rect,
        }
    }
}

/// Watcher -> worker -> overlay wiring.
pub struct Daemon {
    shared: Arc<Shared>,
}

impl Daemon {
    pub fn new(config: Config, overlay: OverlayHandle) -> Self {
        Self {
            shared: Arc::new(Shared {
                config,
                outputs: Mutex::new(HashMap::new()),
                cache: Mutex::new(Cache::new()),
                live_keys: Mutex::new(HashSet::new()),
                dirty: Mutex::new(false),
                cvar: Condvar::new(),
                stop: AtomicBool::new(false),
                overlay,
            }),
        }
    }

    fn request_refresh(shared: &Arc<Shared>) {
        let mut g = lock(&shared.dirty);
        *g = true;
        shared.cvar.notify_one();
    }

    fn refresh_outputs(shared: &Arc<Shared>) {
        match ipc::once(ipc::T_GET_OUTPUTS, "") {
            Ok(outs) => {
                let mut map = HashMap::new();
                if let Some(arr) = outs.as_array() {
                    for o in arr {
                        if let Some(name) = o.get("name").and_then(|n| n.as_str()) {
                            let rect = o.get("rect").cloned().unwrap_or_default();
                            map.insert(name.to_string(), OutRect::from_json(&rect));
                        }
                    }
                }
                *lock(&shared.outputs) = map;
            }
            Err(e) => eprintln!("[daemon] GET_OUTPUTS failed: {e:#}"),
        }
    }

    /// Capture + cache one terminal. None when capture failed (overlay stays hidden).
    fn capture_term(shared: &Arc<Shared>, t: &Term, out_rect: OutRect) -> Option<PendingShow> {
        match capture::capture_and_blur(&t.rect, &shared.config, t.con_id, &t.output, &out_rect) {
            Err(CaptureError(msg)) => {
                println!(
                    "[daemon] capture failed for {}#{}: {msg}",
                    t.app_id, t.con_id,
                );
                flush();
                None
            }
            Ok(img) => {
                lock(&shared.cache).put(t, img.clone(), shared.config.cache_max);
                let r = t.rect;
                println!(
                    "[daemon] SHOW {}#{} @{},{} {}x{}",
                    t.app_id, t.con_id, r.x, r.y, r.width, r.height
                );
                flush();
                Some(PendingShow::of(t.key(), &img, r, out_rect))
            }
        }
    }

    fn refresh_all(shared: &Arc<Shared>) {
        let terms: Vec<Term> = match tree::get_visible_terminals(&shared.config.allow) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[daemon] tree query failed: {e:#}");
                return;
            }
        };
        let overlay = &shared.overlay;
        if terms.is_empty() {
            // Hide + log only on transition, not every timer tick.
            if !lock(&shared.live_keys).is_empty() {
                overlay.hide_all();
                println!("[daemon] HIDE (no visible terminals)");
                flush();
                *lock(&shared.live_keys) = HashSet::new();
            }
            return;
        }
        // Snapshot cache lookup.
        let jobs: Vec<(Term, Option<RgbaImage>)> = {
            let mut cache = lock(&shared.cache);
            terms
                .into_iter()
                .map(|t| {
                    let hit = cache.get_for(&t);
                    (t, hit)
                })
                .collect()
        };
        let keys_now: HashSet<(String, i64)> = jobs.iter().map(|(t, _)| t.key()).collect();
        if jobs.iter().all(|(_, img)| img.is_some()) && keys_now == *lock(&shared.live_keys) {
            // Nothing changed: stay silent.
            return;
        }
        let cached = jobs.iter().filter(|(_, p)| p.is_some()).count();
        println!(
            "[daemon] refresh: {} terminals, {cached} cached",
            jobs.len(),
        );
        flush();
        // A visible overlay would land in another window's screenshot, so when
        // anything needs a fresh capture: hide ALL first, settle once, defer
        // every show until all captures in this pass are done.
        if jobs.iter().any(|(_, img)| img.is_none()) {
            overlay.hide_all();
            // hide presents before capture
            thread::sleep(Duration::from_secs_f64(shared.config.hide_settle_s));
        }
        let outputs = lock(&shared.outputs).clone();
        let mut pending: Vec<PendingShow> = Vec::with_capacity(jobs.len());
        let mut live_keys = Vec::with_capacity(jobs.len());
        for (t, cached_img) in &jobs {
            live_keys.push(t.key());
            let out_rect = outputs.get(&t.output).cloned().unwrap_or_default();
            match cached_img {
                Some(img) => pending.push(PendingShow::of(t.key(), img, t.rect, out_rect)),
                None => {
                    if let Some(show) = Self::capture_term(shared, t, out_rect) {
                        pending.push(show);
                    }
                }
            }
        }
        for show in pending {
            overlay.show(
                show.pixels,
                show.width,
                show.height,
                show.key,
                show.rect,
                show.out_rect,
            );
        }
        overlay.prune(live_keys.iter().cloned().collect());
        *lock(&shared.live_keys) = live_keys.into_iter().collect();
    }

    fn worker_loop(shared: Arc<Shared>) {
        // Timer revalidation: drags emit few/no events, so re-check on a cadence.
        let revalidate = Duration::from_secs_f64(shared.config.revalidate_s);
        let mut last_pass = Instant::now();
        loop {
            {
                let mut g = lock(&shared.dirty);
                while !*g && !shared.stop.load(Ordering::SeqCst) {
                    g = match shared.cvar.wait_timeout(g, Duration::from_millis(100)) {
                        Ok((g, _)) => g,
                        Err(e) => e.into_inner().0,
                    };
                    if !*g && !revalidate.is_zero() && last_pass.elapsed() >= revalidate {
                        *g = true; // timer tick, not an event
                    }
                }
                if shared.stop.load(Ordering::SeqCst) {
                    return;
                }
                *g = false;
            }
            last_pass = Instant::now();
            // Never kill the worker on unexpected errors.
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                Self::refresh_all(&shared);
            }))
            .is_err()
            {
                eprintln!("[daemon] worker error: task panicked");
            }
        }
    }

    /// GTK on the calling thread; IPC + capture on background threads.
    /// `seconds > 0` quits after that long.
    pub fn run(self, seconds: f64) -> Result<()> {
        Self::refresh_outputs(&self.shared);

        let w_shared = Arc::clone(&self.shared);
        let watcher = Watcher::new(&self.shared.config, move || {
            Self::request_refresh(&w_shared);
        });
        let w_thread = Arc::clone(&watcher);
        thread::spawn(move || {
            if let Err(e) = w_thread.run() {
                eprintln!("[watcher] exited: {e:#}");
            }
        });

        let s_worker = Arc::clone(&self.shared);
        thread::spawn(move || Self::worker_loop(s_worker));
        Self::request_refresh(&self.shared); // initial state

        if seconds > 0.0 {
            let s_quit = Arc::clone(&self.shared);
            let ms = (seconds * 1000.0) as u64;
            glib::timeout_add_once(Duration::from_millis(ms), move || {
                watcher.stop();
                s_quit.stop.store(true, Ordering::SeqCst);
                s_quit.cvar.notify_all();
                gtk::main_quit();
            });
        }
        println!("[daemon] running");
        flush();
        gtk::main();
        Ok(())
    }
}

/// --watch-only: print what the daemon WOULD do (no capture/overlay).
pub fn preview_refresh(config: &Config) {
    println!("--- refresh ---");
    match tree::get_visible_terminals(&config.allow) {
        Err(e) => eprintln!("[watch-only] tree query failed: {e:#}"),
        Ok(terms) => {
            if terms.is_empty() {
                println!("HIDE  (no visible terminals)");
            }
            for t in terms {
                let r = t.rect;
                println!(
                    "BLUR  {}#{} on {} @ {},{} {}x{}{}",
                    t.app_id,
                    t.con_id,
                    t.output,
                    r.x,
                    r.y,
                    r.width,
                    r.height,
                    if t.floating { " (floating)" } else { "" }
                );
            }
        }
    }
    flush();
}

/// flock guard: sway `exec_always` reloads can't stack daemons. Caller holds the return.
pub fn acquire_single_instance() -> Result<std::fs::File> {
    use fs2::FileExt;
    let cache = dirs_home_cache()?;
    std::fs::create_dir_all(&cache)?;
    let lock_path = cache.join("sway-blur.lock");
    let fh = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    if fh.try_lock_exclusive().is_err() {
        println!("[daemon] another instance is running, exiting");
        std::process::exit(0);
    }
    use std::io::Write;
    let mut fh = fh;
    let _ = writeln!(fh, "{}", std::process::id());
    let _ = fh.flush();
    Ok(fh)
}

fn dirs_home_cache() -> Result<std::path::PathBuf> {
    if let Some(home) = std::env::var_os("HOME") {
        Ok(std::path::PathBuf::from(home).join(".cache"))
    } else {
        anyhow::bail!("HOME not set")
    }
}
