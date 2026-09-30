use anyhow::{bail, Context, Result};
use std::cell::RefCell;
use std::env;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub const MAGIC: &[u8; 6] = b"i3-ipc";
/// Header: magic(6) + payload_len u32 LE + msg_type u32 LE = 14 bytes.
pub const HDR_SIZE: usize = 14;

pub const T_RUN_COMMAND: u32 = 0;
pub const T_SUBSCRIBE: u32 = 2;
pub const T_GET_OUTPUTS: u32 = 3;
pub const T_GET_TREE: u32 = 4;

pub const EVENT_BIT: u32 = 1 << 31;

/// Per-op IPC budget: sway answers immediately; slower means wedged.
const IO_TIMEOUT: Duration = Duration::from_secs(10);

pub fn connect() -> Result<UnixStream> {
    let path = env::var("SWAYSOCK")
        .map_err(|_| anyhow::anyhow!("SWAYSOCK not set (not inside a sway session?)"))?;
    let s = UnixStream::connect(&path).with_context(|| format!("connecting to SWAYSOCK={path}"))?;
    s.set_read_timeout(Some(IO_TIMEOUT))
        .context("setting sway IPC read timeout")?;
    s.set_write_timeout(Some(IO_TIMEOUT))
        .context("setting sway IPC write timeout")?;
    Ok(s)
}

/// True when `err` is a socket timeout: idle heartbeat, not a dead connection.
pub fn is_timeout(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            )
        })
    })
}

pub fn send(stream: &mut UnixStream, msg_type: u32, payload: &str) -> Result<()> {
    let data = payload.as_bytes();
    let mut buf = Vec::with_capacity(14 + data.len());
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
    buf.extend_from_slice(&msg_type.to_le_bytes());
    buf.extend_from_slice(data);
    stream.write_all(&buf).context("writing sway IPC message")?;
    Ok(())
}

pub fn recv(stream: &mut UnixStream) -> Result<(u32, serde_json::Value)> {
    let mut hdr = [0u8; HDR_SIZE];
    stream.read_exact(&mut hdr).context("sway IPC closed")?;
    if &hdr[0..6] != MAGIC {
        bail!("bad IPC magic: {:?}", &hdr[0..6]);
    }
    let plen = u32::from_le_bytes(hdr[6..10].try_into().unwrap()) as usize;
    let mtype = u32::from_le_bytes(hdr[10..14].try_into().unwrap());
    let mut payload = vec![0u8; plen];
    if plen > 0 {
        stream
            .read_exact(&mut payload)
            .context("sway IPC closed mid-payload")?;
    }
    let text = String::from_utf8(payload).context("sway IPC payload not utf-8")?;
    let v: serde_json::Value = serde_json::from_str(if text.is_empty() { "null" } else { &text })
        .context("parsing sway IPC payload")?;
    Ok((mtype, v))
}

/// One-shot query on a throwaway connection (sway allows many clients).
pub fn once(msg_type: u32, payload: &str) -> Result<serde_json::Value> {
    let mut s = connect()?;
    send(&mut s, msg_type, payload)?;
    Ok(recv(&mut s)?.1)
}

// --- persistent command connection (one per thread, reconnected on failure) ---

thread_local! {
    static CMD_CONN: RefCell<Option<UnixStream>> = const { RefCell::new(None) };
}

/// Query on the thread's persistent connection (no connect/close per call).
pub fn once_reused(msg_type: u32, payload: &str) -> Result<serde_json::Value> {
    CMD_CONN.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(s) = slot.as_mut() {
            let attempt = send(s, msg_type, payload).and_then(|_| recv(s).map(|(_, v)| v));
            if let Ok(v) = attempt {
                return Ok(v);
            }
            // Dead connection: fall through and reconnect.
        }
        let mut s = connect()?;
        send(&mut s, msg_type, payload)?;
        let (_, v) = recv(&mut s)?;
        *slot = Some(s);
        Ok(v)
    })
}
