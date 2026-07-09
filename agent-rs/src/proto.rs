//! Human-side control-socket client. Talks to the running leader over the
//! per-vault UNIX socket (JSON + newline). The agent has **no vault access**;
//! the socket exposes only status/lifecycle (ping/list/close). Launching and
//! any file transfer deliberately do NOT go through this socket — it is
//! human-owned, so any same-uid process can reach it (see the pen-test finding).

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REPLY: usize = 65536;

/// Control-socket path for `vault`. Must match the helper/leader exactly:
/// `$XDG_RUNTIME_DIR/veracage/sessions/<sha256(vault)[:16]>.sock`.
/// The caller must pass the *resolved* vault path (same string the launcher used).
pub fn socket_path(vault: &str) -> io::Result<PathBuf> {
    let runtime = std::env::var("XDG_RUNTIME_DIR")
        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR not set"))?;
    let hex = hex(&Sha256::digest(vault.as_bytes()));
    Ok(PathBuf::from(runtime)
        .join("veracage")
        .join("sessions")
        .join(format!("{}.sock", &hex[..16])))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn connect(vault: &str) -> io::Result<UnixStream> {
    let path = socket_path(vault)?;
    if !path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no active session for {vault}"),
        ));
    }
    let s = UnixStream::connect(path)?;
    s.set_read_timeout(Some(TIMEOUT))?;
    s.set_write_timeout(Some(TIMEOUT))?;
    Ok(s)
}

fn parse(buf: &[u8]) -> io::Result<Value> {
    let text = String::from_utf8_lossy(buf);
    let text = text.trim();
    if text.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Read until newline (the leader terminates every reply with `\n`).
fn read_reply(s: &mut UnixStream) -> io::Result<Value> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = s.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.contains(&b'\n') || buf.len() >= MAX_REPLY {
            break;
        }
    }
    parse(&buf)
}

fn line(req: &Value) -> io::Result<Vec<u8>> {
    let mut v = serde_json::to_vec(req)?;
    v.push(b'\n');
    Ok(v)
}

/// Send one JSON request, return the JSON reply.
pub fn request(vault: &str, req: &Value) -> io::Result<Value> {
    let mut s = connect(vault)?;
    s.write_all(&line(req)?)?;
    read_reply(&mut s)
}

// --- thin command wrappers (mirror leader.py's client fns) ------------------

pub fn ping(vault: &str) -> io::Result<Value> {
    request(vault, &json!({ "cmd": "ping" }))
}
pub fn list(vault: &str) -> io::Result<Value> {
    request(vault, &json!({ "cmd": "list" }))
}
pub fn close(vault: &str) -> io::Result<Value> {
    request(vault, &json!({ "cmd": "close" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_matches_python_sha256_prefix() {
        // sha256("/tmp/veracage-test.vc")[:16] — the hash the launcher logged
        // as `--vault-hash fff2a519a88710fa` for this vault.
        std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");
        let p = socket_path("/tmp/veracage-test.vc").unwrap();
        assert_eq!(
            p,
            PathBuf::from("/run/user/1000/veracage/sessions/fff2a519a88710fa.sock")
        );
    }

    #[test]
    fn parse_handles_empty_and_json() {
        assert_eq!(parse(b"   \n").unwrap(), json!({}));
        assert_eq!(parse(b"{\"ok\":true}\n").unwrap(), json!({"ok": true}));
    }
}
