use crate::capture::{self, CaptureError};
use crate::config::Config;
use crate::ipc;
use crate::model::{ConId, OutRect, SurfaceKey, Term};
use crate::overlay::{Frame, OverlayHandle};
use crate::tree;
use crate::watcher::Watcher;
use anyhow::Result;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
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

struct Snapshot {
    term: Term,
    pixels: Vec<u8>,
    width: u32,
    height: u32,
}

struct CachedFrame {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
}

struct Cache {
    map: HashMap<ConId, Snapshot>,
    order: VecDeque<ConId>,
}

impl Cache {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Freshness is the full term: geometry, output, and the visible windows
    /// behind it. app_id is stable for a con, so including it is harmless.
    fn is_fresh(&self, t: &Term) -> bool {
        self.map.get(&t.con_id).is_some_and(|s| s.term == *t)
    }

    fn promote(&mut self, id: ConId) {
        if let Some(pos) = self.order.iter().position(|&x| x == id) {
            self.order.remove(pos);
            self.order.push_back(id);
        }
    }

    /// Fresh hit: share the pixel buffer and refresh recency.
    fn get(&mut self, t: &Term) -> Option<CachedFrame> {
        if !self.is_fresh(t) {
            return None;
        }
        self.promote(t.con_id);
        let s = self.map.get(&t.con_id)?;
        Some(CachedFrame {
            pixels: s.pixels.clone(),
            width: s.width,
            height: s.height,
        })
    }

    fn put(&mut self, t: &Term, pixels: Vec<u8>, width: u32, height: u32, max: usize) {
        let existed = self.map.contains_key(&t.con_id);
        self.map.insert(
            t.con_id,
            Snapshot {
                term: t.clone(),
                pixels,
                width,
                height,
            },
        );
        if existed {
            self.promote(t.con_id);
        } else {
            self.order.push_back(t.con_id);
        }
        while self.map.len() > max {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            self.map.remove(&old);
        }
    }

    /// Drop snapshots for cons that no longer exist anywhere in the sway
    /// tree (closed windows). Hidden-workspace cons still exist, so switching
    /// back needs no recapture. Returns evicted count.
    fn prune_closed(&mut self, alive: &HashSet<ConId>) -> usize {
        let dead: Vec<ConId> = self
            .map
            .keys()
            .filter(|id| !alive.contains(id))
            .copied()
            .collect();
        let n = dead.len();
        for id in dead {
            self.map.remove(&id);
            if let Some(pos) = self.order.iter().position(|&x| x == id) {
                self.order.remove(pos);
            }
        }
        n
    }
}

/// Worker-owned presentation state: pixel cache + the surface set last sent
/// to the overlay. One lock, one freshness rule.
struct Scene {
    cache: Cache,
    synced: HashSet<SurfaceKey>,
}

impl Scene {
    fn new() -> Self {
        Self {
            cache: Cache::new(),
            synced: HashSet::new(),
        }
    }

    fn is_current(&self, terms: &[Term]) -> bool {
        let keys: HashSet<SurfaceKey> = terms.iter().map(Term::surface_key).collect();
        keys == self.synced && terms.iter().all(|t| self.cache.is_fresh(t))
    }

    fn jobs(&mut self, terms: &[Term]) -> Vec<Job> {
        terms
            .iter()
            .map(|t| Job {
                term: t.clone(),
                cached: self.cache.get(t),
            })
            .collect()
    }
}

struct Job {
    term: Term,
    cached: Option<CachedFrame>,
}

struct Shared {
    config: Config,
    outputs: Mutex<HashMap<String, OutRect>>,
    scene: Mutex<Scene>,
    dirty: Mutex<bool>,
    cvar: Condvar,
    /// Lock-free message handle to the GTK thread.
    overlay: OverlayHandle,
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
                scene: Mutex::new(Scene::new()),
                dirty: Mutex::new(false),
                cvar: Condvar::new(),
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

    /// Capture + blur one terminal. None when capture failed (overlay keeps
    /// the surface hidden and the next pass retries).
    fn capture_term(
        shared: &Arc<Shared>,
        t: &Term,
        out_rect: OutRect,
    ) -> Option<(Vec<u8>, u32, u32)> {
        match capture::capture_and_blur(&t.rect, &shared.config, t.con_id.0, &t.output, &out_rect) {
            Err(CaptureError(msg)) => {
                println!(
                    "[daemon] capture failed for {}#{}: {msg}",
                    t.app_id, t.con_id,
                );
                flush();
                None
            }
            Ok(img) => {
                let (width, height) = img.dimensions();
                let pixels = img.into_raw();
                println!(
                    "[daemon] SHOW {}#{} @{},{} {}x{}",
                    t.app_id, t.con_id, t.rect.x, t.rect.y, t.rect.width, t.rect.height
                );
                flush();
                Some((pixels, width, height))
            }
        }
    }

    fn refresh_all(shared: &Arc<Shared>) {
        let (terms, all_ids): (Vec<Term>, HashSet<ConId>) =
            match tree::get_visible_terminals_and_all_ids(&shared.config.allow) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("[daemon] tree query failed: {e:#}");
                    return;
                }
            };

        let jobs: Vec<Job> = {
            let mut scene = lock(&shared.scene);
            // Free pixel buffers of truly closed windows on every pass.
            // Hidden-workspace ids are still alive, so switch-back stays cached.
            let evicted = scene.cache.prune_closed(&all_ids);
            if evicted > 0 {
                println!("[daemon] evict {evicted} closed snapshots");
                flush();
            }

            if terms.is_empty() {
                // Hide + destroy only on transition, not every timer tick.
                if scene.synced.is_empty() {
                    return;
                }
                scene.synced.clear();
                drop(scene);
                shared.overlay.sync(Vec::new());
                println!("[daemon] HIDE (no visible terminals)");
                flush();
                return;
            }

            if scene.is_current(&terms) {
                // Nothing changed: stay silent.
                return;
            }
            scene.jobs(&terms)
        };

        let cached = jobs.iter().filter(|j| j.cached.is_some()).count();
        println!(
            "[daemon] refresh: {} terminals, {cached} cached",
            jobs.len(),
        );
        flush();

        // A visible overlay would land in another window's screenshot, so when
        // anything needs a fresh capture: hide ALL first, settle once, defer
        // every show until all captures in this pass are done.
        if jobs.iter().any(|j| j.cached.is_none()) {
            shared.overlay.hide_all();
            thread::sleep(Duration::from_secs_f64(shared.config.hide_settle_s));
        }

        let mut frames: Vec<Frame> = Vec::with_capacity(jobs.len());
        let mut new_snapshots: Vec<(Term, Vec<u8>, u32, u32)> = Vec::new();
        for job in jobs {
            // Short lock per window instead of cloning the whole output map.
            let out_rect = lock(&shared.outputs)
                .get(&job.term.output)
                .copied()
                .unwrap_or_default();
            let key = job.term.surface_key();
            match job.cached {
                Some(c) => frames.push(Frame {
                    key,
                    pixels: Some(c.pixels),
                    width: c.width as i32,
                    height: c.height as i32,
                    rect: job.term.rect,
                    out_rect,
                }),
                None => {
                    if let Some((pixels, width, height)) =
                        Self::capture_term(shared, &job.term, out_rect)
                    {
                        frames.push(Frame {
                            key,
                            pixels: Some(pixels.clone()),
                            width: width as i32,
                            height: height as i32,
                            rect: job.term.rect,
                            out_rect,
                        });
                        new_snapshots.push((job.term, pixels, width, height));
                    } else {
                        // Keep the surface (hidden) so the next pass can retry
                        // without recreating the GTK window.
                        frames.push(Frame {
                            key,
                            pixels: None,
                            width: 0,
                            height: 0,
                            rect: job.term.rect,
                            out_rect,
                        });
                    }
                }
            }
        }

        let keys: HashSet<SurfaceKey> = frames.iter().map(|f| f.key.clone()).collect();
        shared.overlay.sync(frames);
        {
            let mut scene = lock(&shared.scene);
            for (t, pixels, width, height) in new_snapshots {
                scene
                    .cache
                    .put(&t, pixels, width, height, shared.config.cache_max);
            }
            scene.synced = keys;
        }
    }

    fn worker_loop(shared: Arc<Shared>) {
        // Timer revalidation: drags emit few/no events, so re-check on a cadence.
        let revalidate = Duration::from_secs_f64(shared.config.revalidate_s);
        let mut last_pass = Instant::now();
        loop {
            {
                let mut g = lock(&shared.dirty);
                while !*g {
                    g = match shared.cvar.wait_timeout(g, Duration::from_millis(100)) {
                        Ok((g, _)) => g,
                        Err(e) => e.into_inner().0,
                    };
                    if !*g && !revalidate.is_zero() && last_pass.elapsed() >= revalidate {
                        *g = true; // timer tick, not an event
                    }
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
    pub fn run(self) -> Result<()> {
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

        println!("[daemon] running");
        flush();
        gtk::main();
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Below, Rect};

    fn rect(x: i32, y: i32, width: i32, height: i32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn term(con_id: i64, r: Rect, below: Vec<Below>) -> Term {
        Term {
            con_id: ConId(con_id),
            app_id: "foot".to_string(),
            rect: r,
            output: "DP-1".to_string(),
            below,
        }
    }

    fn px(marker: u8) -> Vec<u8> {
        vec![marker; 4]
    }

    #[test]
    fn freshness_tracks_geometry_and_below() {
        let mut cache = Cache::new();
        let t = term(1, rect(0, 0, 10, 10), Vec::new());
        cache.put(&t, px(1), 1, 1, 4);
        assert!(cache.is_fresh(&t));
        assert!(cache.get(&t).is_some());

        let moved = term(1, rect(5, 0, 10, 10), Vec::new());
        assert!(!cache.is_fresh(&moved));
        assert!(cache.get(&moved).is_none());

        let behind = term(
            1,
            rect(0, 0, 10, 10),
            vec![Below {
                con_id: ConId(2),
                rect: rect(1, 1, 2, 2),
            }],
        );
        assert!(!cache.is_fresh(&behind));
    }

    #[test]
    fn put_promotes_existing_and_evicts_lru() {
        let mut cache = Cache::new();
        let t1 = term(1, rect(0, 0, 10, 10), Vec::new());
        let t2 = term(2, rect(0, 0, 10, 10), Vec::new());
        let t3 = term(3, rect(0, 0, 10, 10), Vec::new());
        let t4 = term(4, rect(0, 0, 10, 10), Vec::new());

        cache.put(&t1, px(1), 1, 1, 2);
        cache.put(&t2, px(2), 1, 1, 2);
        assert!(cache.get(&t1).is_some()); // order: t2, t1
        cache.put(&t3, px(3), 1, 1, 2); // evict t2
        assert!(cache.is_fresh(&t1));
        assert!(!cache.is_fresh(&t2));
        assert!(cache.is_fresh(&t3));
        cache.put(&t1, px(1), 1, 1, 2); // existing item promoted: t3, t1
        cache.put(&t4, px(4), 1, 1, 2); // evict t3
        assert!(cache.is_fresh(&t1));
        assert!(!cache.is_fresh(&t3));
        assert!(cache.is_fresh(&t4));
    }

    #[test]
    fn prune_keeps_alive_including_hidden_workspaces() {
        let mut cache = Cache::new();
        let t1 = term(1, rect(0, 0, 10, 10), Vec::new());
        let t2 = term(2, rect(0, 0, 10, 10), Vec::new());
        cache.put(&t1, px(1), 1, 1, 4);
        cache.put(&t2, px(2), 1, 1, 4);

        let alive = HashSet::from([ConId(1)]);
        assert_eq!(cache.prune_closed(&alive), 1);
        assert!(cache.is_fresh(&t1));
        assert!(!cache.is_fresh(&t2));
    }

    #[test]
    fn scene_requires_same_surfaces_and_fresh_frames() {
        let mut scene = Scene::new();
        let t = term(1, rect(0, 0, 10, 10), Vec::new());
        scene.cache.put(&t, px(1), 1, 1, 4);
        scene.synced.insert(t.surface_key());
        assert!(scene.is_current(std::slice::from_ref(&t)));
        scene.synced.clear();
        assert!(!scene.is_current(std::slice::from_ref(&t)));
    }
}
