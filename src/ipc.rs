//! Local IPC: Unix-socket transport, NDJSON framing, the request/response
//! protocol, and a generic request/response server.
//!
//! Per ADR 0007 this is **synchronous** — threads + channels, no async runtime.
//! The daemon runs [`serve`] on its own thread; a local CLI connects with a
//! [`Client`]. The wire format is NDJSON: one JSON object per `\n`-terminated
//! line. This is an *internal* contract (ADR 0002 freezes only `--format json`),
//! so it may evolve without an agent-surface guarantee.
//!
//! The transport layer speaks [`std::io::Result`]: I/O failures surface as
//! `io::Error`, while *protocol* errors (an unknown method, bad params) ride
//! back inside a [`Response`] payload, never as a transport error.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Directory mode for the runtime dir: owner rwx only (single-user isolation).
const DIR_MODE: u32 = 0o700;
/// Socket file mode: owner rw only.
const SOCK_MODE: u32 = 0o600;

/// A single request. `params` is method-specific and left opaque here so the
/// transport stays agnostic to the method set (defined by the daemon handler).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct Request {
    pub(crate) id: String,
    pub(crate) method: String,
    pub(crate) params: serde_json::Value,
}

/// The structured error body of a failed [`Response`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct ErrorBody {
    pub(crate) code: String,
    pub(crate) message: String,
}

/// A response to a [`Request`], discriminated on the `ok` field.
///
/// Success serializes to `{ "id", "ok": true, "result": … }`; failure to
/// `{ "id", "ok": false, "error": { "code", "message" } }`. The untagged enum
/// plus the matching `ok` literal in each arm produce exactly those shapes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub(crate) enum Response {
    Ok {
        id: String,
        /// Always `true`; pins the success shape and discriminates on the wire.
        ok: AlwaysTrue,
        result: serde_json::Value,
    },
    Err {
        id: String,
        /// Always `false`; pins the error shape and discriminates on the wire.
        ok: AlwaysFalse,
        error: ErrorBody,
    },
}

#[allow(dead_code)] // wired by the daemon in PDC-74
impl Response {
    /// Build a success response carrying `result`.
    pub(crate) fn ok(id: impl Into<String>, result: serde_json::Value) -> Self {
        Response::Ok {
            id: id.into(),
            ok: AlwaysTrue,
            result,
        }
    }

    /// Build an error response with a machine `code` and human `message`.
    pub(crate) fn err(
        id: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Response::Err {
            id: id.into(),
            ok: AlwaysFalse,
            error: ErrorBody {
                code: code.into(),
                message: message.into(),
            },
        }
    }
}

/// A unit type that serializes to the JSON boolean `true`. Lets the success
/// arm of [`Response`] carry an `"ok": true` literal without a free `bool` that
/// `serde(untagged)` could mis-match against the error arm.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct AlwaysTrue;

impl Serialize for AlwaysTrue {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bool(true)
    }
}

impl<'de> Deserialize<'de> for AlwaysTrue {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        if bool::deserialize(d)? {
            Ok(AlwaysTrue)
        } else {
            Err(serde::de::Error::custom("expected `ok` to be true"))
        }
    }
}

/// A unit type that serializes to the JSON boolean `false`. See [`AlwaysTrue`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct AlwaysFalse;

impl Serialize for AlwaysFalse {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bool(false)
    }
}

impl<'de> Deserialize<'de> for AlwaysFalse {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        if bool::deserialize(d)? {
            Err(serde::de::Error::custom("expected `ok` to be false"))
        } else {
            Ok(AlwaysFalse)
        }
    }
}

/// Resolve the daemon's socket path.
///
/// Prefers `$XDG_RUNTIME_DIR/periodic/periodic.sock`; when `XDG_RUNTIME_DIR` is
/// unset (or empty), falls back to `$HOME/.config/periodic/run/periodic.sock`.
#[allow(dead_code)] // wired by the daemon in PDC-74
pub(crate) fn socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("periodic/periodic.sock"),
        _ => {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default();
            home.join(".config/periodic/run/periodic.sock")
        }
    }
}

/// Write one message as an NDJSON line: the JSON encoding of `msg` followed by
/// `\n`. Flushes so a peer blocked on a line read makes progress.
pub(crate) fn write_message<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()
}

/// Read one NDJSON message. Returns `Ok(None)` at clean EOF (no bytes before
/// the stream closed), `Ok(Some(_))` for a parsed line, or an error for I/O or
/// malformed JSON.
pub(crate) fn read_message<R: BufRead, T: for<'de> Deserialize<'de>>(
    r: &mut R,
) -> io::Result<Option<T>> {
    let mut line = String::new();
    let n = r.read_line(&mut line)?;
    if n == 0 {
        return Ok(None);
    }
    let msg = serde_json::from_str(line.trim_end_matches('\n'))?;
    Ok(Some(msg))
}

/// Bind the listener at `path`, creating the parent dir mode `0700` and the
/// socket file mode `0600`. Removes any stale socket file first.
fn bind(path: &Path) -> io::Result<UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(DIR_MODE))?;
    }
    // A leftover socket from a crashed daemon would make bind() fail with
    // EADDRINUSE; clear it. (Ignore "not found".)
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(SOCK_MODE))?;
    Ok(listener)
}

/// Run the IPC server until `stop` is set, dispatching each request through
/// `handler`.
///
/// **Stop mechanism:** the accept loop polls `stop` between accepts. The
/// listener is put in non-blocking mode and `accept()` is retried on
/// `WouldBlock` with a short sleep, so a daemon can flip `stop` from another
/// thread and the loop exits promptly without needing a self-connect wake-up.
/// On return the socket file is removed.
///
/// Each accepted connection is served sequentially on the calling thread: its
/// NDJSON requests are read until EOF, each answered with one response line.
/// The client is a local CLI, so per-connection concurrency is unnecessary.
#[allow(dead_code)] // wired by the daemon in PDC-74
pub(crate) fn serve<F>(path: &Path, stop: &AtomicBool, handler: F) -> io::Result<()>
where
    F: Fn(Request) -> Response,
{
    let listener = bind(path)?;
    listener.set_nonblocking(true)?;
    let result = accept_loop(&listener, stop, &handler);
    let _ = std::fs::remove_file(path);
    result
}

fn accept_loop<F>(listener: &UnixListener, stop: &AtomicBool, handler: &F) -> io::Result<()>
where
    F: Fn(Request) -> Response,
{
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _addr)) => {
                // Restore blocking on the accepted stream for clean line reads.
                stream.set_nonblocking(false)?;
                if let Err(e) = serve_connection(stream, handler) {
                    tracing::warn!(error = %e, "ipc connection error");
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Handle one connection: read requests until EOF, answer each in order.
fn serve_connection<F>(stream: UnixStream, handler: &F) -> io::Result<()>
where
    F: Fn(Request) -> Response,
{
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    while let Some(req) = read_message::<_, Request>(&mut reader)? {
        let resp = handler(req);
        write_message(&mut writer, &resp)?;
    }
    Ok(())
}

/// A connected IPC client holding one stream. [`Client::call`] is one
/// request / one response, but multiple calls may reuse the same connection.
#[allow(dead_code)] // wired by the daemon in PDC-74
pub(crate) struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

#[allow(dead_code)] // wired by the daemon in PDC-74
impl Client {
    /// Connect to the daemon socket at `path`.
    pub(crate) fn connect(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        let writer = stream.try_clone()?;
        Ok(Client {
            reader: BufReader::new(stream),
            writer,
        })
    }

    /// Send `req` and read the single response. An EOF before any response
    /// surfaces as an `UnexpectedEof` error.
    pub(crate) fn call(&mut self, req: &Request) -> io::Result<Response> {
        write_message(&mut self.writer, req)?;
        match read_message::<_, Response>(&mut self.reader)? {
            Some(resp) => Ok(resp),
            None => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before response",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn request_serializes_to_wire_shape() {
        let req = Request {
            id: "1".into(),
            method: "jobs.list".into(),
            params: json!({"all": true}),
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(
            v,
            json!({"id": "1", "method": "jobs.list", "params": {"all": true}})
        );
    }

    #[test]
    fn ok_response_serializes_to_wire_shape() {
        let resp = Response::ok("1", json!({"count": 3}));
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert_eq!(v, json!({"id": "1", "ok": true, "result": {"count": 3}}));
    }

    #[test]
    fn err_response_serializes_to_wire_shape() {
        let resp = Response::err("1", "not_found", "no such job: x");
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert_eq!(
            v,
            json!({"id": "1", "ok": false, "error": {"code": "not_found", "message": "no such job: x"}})
        );
    }

    #[test]
    fn response_round_trips_both_variants() {
        let ok = Response::ok("a", json!([1, 2]));
        let back: Response = serde_json::from_str(&serde_json::to_string(&ok).unwrap()).unwrap();
        assert_eq!(ok, back);

        let err = Response::err("b", "bad", "boom");
        let back: Response = serde_json::from_str(&serde_json::to_string(&err).unwrap()).unwrap();
        assert_eq!(err, back);
    }

    #[test]
    fn write_then_read_round_trips_one_message() {
        let req = Request {
            id: "7".into(),
            method: "ping".into(),
            params: json!(null),
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &req).unwrap();
        assert_eq!(buf.last(), Some(&b'\n'));
        let mut reader = Cursor::new(buf);
        let got: Option<Request> = read_message(&mut reader).unwrap();
        assert_eq!(got, Some(req));
    }

    #[test]
    fn two_messages_read_back_in_order() {
        let a = Request {
            id: "1".into(),
            method: "m1".into(),
            params: json!(1),
        };
        let b = Request {
            id: "2".into(),
            method: "m2".into(),
            params: json!(2),
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &a).unwrap();
        write_message(&mut buf, &b).unwrap();
        let mut reader = Cursor::new(buf);
        let r1: Option<Request> = read_message(&mut reader).unwrap();
        let r2: Option<Request> = read_message(&mut reader).unwrap();
        let r3: Option<Request> = read_message(&mut reader).unwrap();
        assert_eq!(r1, Some(a));
        assert_eq!(r2, Some(b));
        assert_eq!(r3, None);
    }

    #[test]
    fn read_message_returns_none_at_eof() {
        let mut reader = Cursor::new(Vec::new());
        let got: Option<Request> = read_message(&mut reader).unwrap();
        assert_eq!(got, None);
    }

    #[test]
    fn socket_path_prefers_xdg_runtime_dir() {
        with_runtime_dir(Some("/run/user/1000"), || {
            assert_eq!(
                socket_path(),
                PathBuf::from("/run/user/1000/periodic/periodic.sock")
            );
        });
    }

    #[test]
    fn socket_path_falls_back_to_home_when_runtime_unset() {
        with_runtime_dir(None, || {
            let p = socket_path();
            assert!(
                p.ends_with(".config/periodic/run/periodic.sock"),
                "got {p:?}"
            );
        });
    }

    /// Set (or clear) `XDG_RUNTIME_DIR` for the duration of `f`, then restore.
    /// A process-global mutex serializes the two env-touching tests.
    fn with_runtime_dir(val: Option<&str>, f: impl FnOnce()) {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("XDG_RUNTIME_DIR");
        // SAFETY: guarded by LOCK; no other thread mutates the env concurrently.
        unsafe {
            match val {
                Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
        }
        f();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
        }
    }

    /// Block until `Client::connect(path)` succeeds, then return the client.
    fn connect_when_ready(path: &Path) -> Client {
        loop {
            match Client::connect(path) {
                Ok(c) => break c,
                Err(_) => thread::sleep(Duration::from_millis(5)),
            }
        }
    }

    #[test]
    fn end_to_end_over_real_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("nested/periodic.sock");
        let stop = Arc::new(AtomicBool::new(false));

        let server_sock = sock.clone();
        let server_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            serve(&server_sock, &server_stop, |req| {
                // Echo handler: bounce params back, or error on a sentinel method.
                if req.method == "boom" {
                    Response::err(req.id, "boom", "exploded")
                } else {
                    Response::ok(req.id, req.params)
                }
            })
            .unwrap();
        });

        let mut client = connect_when_ready(&sock);

        let ok = client
            .call(&Request {
                id: "1".into(),
                method: "echo".into(),
                params: json!({"hi": "there"}),
            })
            .unwrap();
        assert_eq!(ok, Response::ok("1", json!({"hi": "there"})));

        // A second request on the same connection.
        let err = client
            .call(&Request {
                id: "2".into(),
                method: "boom".into(),
                params: json!(null),
            })
            .unwrap();
        assert_eq!(err, Response::err("2", "boom", "exploded"));

        drop(client);
        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();
        // serve() removes the socket on exit.
        assert!(!sock.exists());
    }

    #[test]
    fn socket_and_dir_have_restrictive_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("rundir/periodic.sock");
        let stop = Arc::new(AtomicBool::new(false));

        let server_sock = sock.clone();
        let server_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            serve(&server_sock, &server_stop, |req| {
                Response::ok(req.id, req.params)
            })
            .unwrap();
        });

        // Connect so the socket is bound before we inspect its mode.
        let client = connect_when_ready(&sock);

        let sock_mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
        let dir_mode = std::fs::metadata(sock.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(sock_mode, SOCK_MODE, "socket should be 0600");
        assert_eq!(dir_mode, DIR_MODE, "run dir should be 0700");

        drop(client);
        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();
    }
}
