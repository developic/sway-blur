use crate::config::Config;
use crate::ipc;
use anyhow::{Context, Result};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

/// Sway event subscription with debounced `on_change()`. Any event just means
/// "maybe changed"; the snapshot cache decides what really recaptures.
pub struct Watcher {
    debounce: Duration,
    on_change: Arc<dyn Fn() + Send + Sync>,
    /// Last kick time; `None` means clean. One field, no invalid states.
    dirty_since: Mutex<Option<Instant>>,
    stop: AtomicBool,
}

impl Watcher {
    pub fn new<F>(config: &Config, on_change: F) -> Arc<Self>
    where
        F: Fn() + Send + Sync + 'static,
    {
        Arc::new(Self {
            debounce: Duration::from_secs_f64(config.debounce_s),
            on_change: Arc::new(on_change),
            dirty_since: Mutex::new(None),
            stop: AtomicBool::new(false),
        })
    }

    fn kick(self: &Arc<Self>) {
        let mut g = self.dirty_since.lock().unwrap_or_else(|e| e.into_inner());
        *g = Some(Instant::now());
    }

    fn debounce_loop(self: Arc<Self>) {
        while !self.stop.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(50));
            let fire = {
                let mut g = self.dirty_since.lock().unwrap_or_else(|e| e.into_inner());
                match *g {
                    Some(at) if at.elapsed() >= self.debounce => {
                        *g = None;
                        true
                    }
                    _ => false,
                }
            };
            if fire {
                let cb = Arc::clone(&self.on_change);
                // Never kill the loop on IPC hiccups — catch panics too.
                if let Err(payload) =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb()))
                {
                    let what = payload
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_else(|| "?".to_string());
                    eprintln!("[watcher] on_change panicked: {what}");
                }
            }
        }
    }

    /// Blocking subscribe loop for a background thread. Dead subscriptions retry.
    pub fn run(self: &Arc<Self>) -> Result<()> {
        let this = Arc::clone(self);
        thread::spawn(move || this.debounce_loop());
        while !self.stop.load(Ordering::SeqCst) {
            match self.subscribe_once() {
                Ok(()) => {}
                Err(e) => {
                    if self.stop.load(Ordering::SeqCst) {
                        break;
                    }
                    eprintln!("[watcher] subscribe failed ({e:#}); retrying…");
                    // Failures are rare: one plain sleep, then re-check stop.
                    thread::sleep(Duration::from_secs(1));
                }
            }
        }
        Ok(())
    }

    /// One subscription lifetime. Idle 10s read timeouts are a healthy heartbeat.
    fn subscribe_once(self: &Arc<Self>) -> Result<()> {
        let mut sock = ipc::connect()?;
        ipc::send(
            &mut sock,
            ipc::T_SUBSCRIBE,
            r#"["window","workspace","output"]"#,
        )?;
        let (_mtype, reply) = ipc::recv(&mut sock)?;
        if reply.get("success").and_then(|s| s.as_bool()) != Some(true) {
            anyhow::bail!("subscribe failed: {reply}");
        }
        println!("[watcher] subscribed to window/workspace/output");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        self.kick(); // initial state: capture everything
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            match ipc::recv(&mut sock) {
                Ok((mtype, _payload)) => {
                    if mtype & ipc::EVENT_BIT != 0 {
                        self.kick();
                    }
                }
                Err(e) if ipc::is_timeout(&e) => continue,
                Err(e) => return Err(e).context("watcher IPC recv"),
            }
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}
