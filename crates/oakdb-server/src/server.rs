//! The server: one thread per connection (ADR 0010, design 0020).
//!
//! The accepting thread checks each new client against the allowlist and
//! the connection limit, then hands it a thread of its own. That thread
//! reads the HELLO, answers READY, and then runs each QUERY through
//! `oakdb_sql::execute` (one statement, one transaction) and writes the
//! response, until the client leaves. Connections share the one database.
//!
//! Not here yet, by design: closing idle connections (#37) and a way to
//! stop the server (#38). `run` takes connections until the process ends.

use std::io::{self, BufReader, Read};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use oakdb::fs::Fs;
use oakdb::Db;

use crate::allow::Allowlist;
use crate::protocol::{
    closes, code, error_code, read_frame, write_frame, DecodeError, FrameError, Request, Response,
    VERSION,
};
use crate::rlimit::raise_fd_limit;

/// The port the server listens on unless told otherwise.
pub const DEFAULT_PORT: u16 = 7878;

/// How many connections the server takes at once unless told otherwise:
/// "several hundred clients" with room to spare (design 0020, 1-1).
pub const DEFAULT_MAX_CONNECTIONS: usize = 512;

/// Stack of a connection thread. Its calls are shallow and large values
/// live on the heap, so the 2 MiB default would only reserve address
/// space: 512 of them is 1 GiB (ADR 0010).
pub const CONNECTION_STACK: usize = 256 * 1024;

/// File descriptors kept beyond one per connection: the listener, the
/// database's files, standard streams, refused connections lingering.
pub const FD_MARGIN: u64 = 64;

/// How long a refused or broken connection is given to take its error
/// before the socket closes.
const LINGER: Duration = Duration::from_secs(1);

/// Refused connections that may linger at once, each on a small thread of
/// its own. Past this a refusal closes at once, as before (review P3).
const MAX_LINGERING: usize = 32;

/// Stack of a thread that only lingers on a refused connection.
const LINGER_STACK: usize = 64 * 1024;

/// How the server runs.
#[derive(Debug, Clone)]
pub struct Config {
    /// TCP port; 0 lets the operating system pick one.
    pub port: u16,
    /// Who may connect. It also picks the address to listen on.
    pub allow: Allowlist,
    /// Connections taken at once. One more is refused with
    /// [`code::TOO_MANY_CONNECTIONS`].
    pub max_connections: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            port: DEFAULT_PORT,
            allow: Allowlist::default(),
            max_connections: DEFAULT_MAX_CONNECTIONS,
        }
    }
}

/// A server bound to its port, not yet taking connections.
pub struct Server<F: Fs> {
    listener: TcpListener,
    shared: Arc<Shared<F>>,
    /// Refused connections lingering now.
    lingering: Arc<AtomicUsize>,
}

/// What every connection thread shares.
struct Shared<F: Fs> {
    db: Db<F>,
    allow: Allowlist,
    max_connections: usize,
    active: AtomicUsize,
    /// Set once the engine's stop is logged, to log it once.
    stop_logged: AtomicBool,
}

impl<F: Fs + 'static> Server<F>
where
    Db<F>: Send + Sync,
{
    /// Raises the file descriptor limit to fit `max_connections`, and binds
    /// the port on the address the allowlist picks. Fails, saying what to
    /// do, if the limit cannot be raised that far.
    pub fn bind(db: Db<F>, config: Config) -> io::Result<Self> {
        let need = config.max_connections as u64 + FD_MARGIN;
        let got = raise_fd_limit(need)?;
        if got < need {
            return Err(io::Error::other(format!(
                "{} connections need {need} open files, but the limit is {got}; \
                 raise `ulimit -n` or lower --max-connections",
                config.max_connections
            )));
        }
        let listener = TcpListener::bind((config.allow.bind_ip(), config.port))?;
        Ok(Server {
            listener,
            shared: Arc::new(Shared {
                db,
                allow: config.allow,
                max_connections: config.max_connections,
                active: AtomicUsize::new(0),
                stop_logged: AtomicBool::new(false),
            }),
            lingering: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// The address the server listens on, with the real port.
    pub fn local_addr(&self) -> SocketAddr {
        self.listener
            .local_addr()
            .expect("a bound listener has an address")
    }

    /// Takes connections until the process ends.
    pub fn run(self) {
        for conn in self.listener.incoming() {
            let stream = match conn {
                Ok(stream) => stream,
                Err(e) => {
                    // Out of descriptors and the like: wait instead of
                    // spinning.
                    eprintln!("oakdb: accepting a connection failed: {e}");
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
            };
            let Ok(peer) = stream.peer_addr() else {
                continue;
            };
            if !self.shared.allow.allows(peer.ip()) {
                eprintln!("oakdb: refused {peer}: not on the allowlist");
                let message = format!("address {} is not allowed", peer.ip());
                self.refuse(stream, code::NOT_ALLOWED, message);
                continue;
            }
            let Some(slot) = Slot::take(&self.shared) else {
                let max = self.shared.max_connections;
                eprintln!("oakdb: refused {peer}: {max} connections already");
                let message = format!("too many connections (max {max})");
                self.refuse(stream, code::TOO_MANY_CONNECTIONS, message);
                continue;
            };
            let spawned = thread::Builder::new()
                .name(format!("oakdb {peer}"))
                .stack_size(CONNECTION_STACK)
                .spawn(move || {
                    eprintln!("oakdb: {peer} connected");
                    serve_guarded(peer, &stream, || serve(&slot.shared, &stream));
                    eprintln!("oakdb: {peer} disconnected");
                    drop(slot);
                });
            // On failure the closure is dropped, which frees the slot and
            // closes the socket.
            if let Err(e) = spawned {
                eprintln!("oakdb: no thread for {peer}: {e}");
            }
        }
    }

    /// Sends an error to a client turned away at the door, and closes once
    /// it has had a moment to read it. The waiting happens on a small
    /// thread of its own, so the accepting thread never waits; past
    /// [`MAX_LINGERING`] of them the socket closes at once, after reading
    /// off what has already arrived.
    fn refuse(&self, stream: TcpStream, code: u8, message: String) {
        let _ = stream.set_write_timeout(Some(LINGER));
        let mut writer = &stream;
        let _ = write_frame(&mut writer, &error_frame(code, message));
        let _ = stream.shutdown(Shutdown::Write);
        if let Some(turn) = Lingering::take(&self.lingering) {
            // If no thread can be had, the closure is dropped and the
            // socket closes at once.
            let _ = thread::Builder::new()
                .name("oakdb refusal".into())
                .stack_size(LINGER_STACK)
                .spawn(move || {
                    linger(&stream);
                    drop(turn);
                });
        } else if stream.set_nonblocking(true).is_ok() {
            let mut sink = [0u8; 4096];
            while matches!((&stream).read(&mut sink), Ok(n) if n > 0) {}
        }
    }
}

/// One of the `max_connections` seats, given back when dropped.
struct Slot<F: Fs> {
    shared: Arc<Shared<F>>,
}

impl<F: Fs> Slot<F> {
    fn take(shared: &Arc<Shared<F>>) -> Option<Self> {
        if shared.active.fetch_add(1, Ordering::AcqRel) >= shared.max_connections {
            shared.active.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(Slot {
            shared: Arc::clone(shared),
        })
    }
}

impl<F: Fs> Drop for Slot<F> {
    fn drop(&mut self) {
        self.shared.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// One of the [`MAX_LINGERING`] turns to linger, given back when dropped.
struct Lingering(Arc<AtomicUsize>);

impl Lingering {
    fn take(count: &Arc<AtomicUsize>) -> Option<Self> {
        if count.fetch_add(1, Ordering::AcqRel) >= MAX_LINGERING {
            count.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(Lingering(Arc::clone(count)))
    }
}

impl Drop for Lingering {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// An `ERROR` frame. Messages are short, so it always fits.
fn error_frame(code: u8, message: String) -> crate::protocol::Frame {
    Response::Error { code, message }
        .encode()
        .expect("an error message fits in a frame")
}

/// What the connection does after an error.
enum Next {
    GoOn,
    Close,
}

/// Sends an `ERROR`. After a code that [`closes`] the connection, it waits
/// for the client to read it and says to close.
/// Runs `serve` for the connection from `peer`; if it panics, the client
/// gets [`code::INTERNAL`] before the socket closes, and the server goes on.
fn serve_guarded(peer: SocketAddr, stream: &TcpStream, serve: impl FnOnce() -> io::Result<()>) {
    match panic::catch_unwind(AssertUnwindSafe(serve)) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("oakdb: connection from {peer}: {e}"),
        Err(_) => {
            // The panic hook has printed what happened.
            eprintln!("oakdb: connection from {peer} failed inside the server");
            let message = "the server failed on this connection".to_string();
            let _ = send_error(stream, code::INTERNAL, message);
        }
    }
}

fn send_error(stream: &TcpStream, code: u8, message: String) -> io::Result<Next> {
    let mut writer = stream;
    write_frame(&mut writer, &error_frame(code, message))?;
    if closes(code) {
        linger(stream);
        Ok(Next::Close)
    } else {
        Ok(Next::GoOn)
    }
}

/// Sends a response, or [`code::RESULT_TOO_LARGE`] if it cannot be sent as
/// a frame.
fn send(stream: &TcpStream, response: &Response) -> io::Result<()> {
    match response.encode() {
        Ok(frame) => {
            let mut writer = stream;
            write_frame(&mut writer, &frame)
        }
        Err(e) => send_error(stream, code::RESULT_TOO_LARGE, e.message).map(|_| ()),
    }
}

/// Stops writing and reads what the client still sends, for up to
/// [`LINGER`], so the close does not reset the connection and throw away
/// the error it was just sent.
fn linger(stream: &TcpStream) {
    let _ = stream.shutdown(Shutdown::Write);
    let deadline = Instant::now() + LINGER;
    let _ = stream.set_read_timeout(Some(LINGER));
    // Small enough for the LINGER_STACK thread this runs on: a buffer the
    // size of the whole stack overflowed it in debug builds on Linux.
    let mut sink = [0u8; 4096];
    let mut reader = stream;
    while Instant::now() < deadline {
        match reader.read(&mut sink) {
            Ok(n) if n > 0 => {}
            _ => break,
        }
    }
}

/// What to do after a frame could not be read.
fn frame_failed(stream: &TcpStream, e: FrameError) -> io::Result<()> {
    match e {
        FrameError::Empty => send_error(stream, code::MALFORMED, e.to_string()).map(|_| ()),
        FrameError::TooLarge(_) => {
            send_error(stream, code::FRAME_TOO_LARGE, e.to_string()).map(|_| ())
        }
        // The client left, mid-frame or with an error: nobody to tell.
        FrameError::Truncated => Ok(()),
        FrameError::Io(e) => Err(e),
    }
}

/// Runs one connection to its end.
fn serve<F: Fs>(shared: &Shared<F>, stream: &TcpStream) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let mut reader = BufReader::new(stream);

    // The handshake.
    let frame = match read_frame(&mut reader) {
        Ok(Some(frame)) => frame,
        Ok(None) => return Ok(()),
        Err(e) => return frame_failed(stream, e),
    };
    let refused = match Request::decode(frame) {
        Ok(Request::Hello { version: VERSION }) => None,
        Ok(Request::Hello { version }) => Some(format!(
            "the client speaks protocol {version}, this server speaks {VERSION}"
        )),
        Err(DecodeError::BadMagic) => Some("not an Oakdb client".to_string()),
        _ => Some("the first frame must be HELLO".to_string()),
    };
    if let Some(message) = refused {
        return send_error(stream, code::HANDSHAKE, message).map(|_| ());
    }
    send(stream, &Response::Ready { version: VERSION })?;

    loop {
        let frame = match read_frame(&mut reader) {
            Ok(Some(frame)) => frame,
            Ok(None) => return Ok(()),
            Err(e) => return frame_failed(stream, e),
        };
        let failed = match Request::decode(frame) {
            Ok(Request::Query(sql)) => {
                send(stream, &run(shared, &sql))?;
                continue;
            }
            Err(DecodeError::NotUtf8) => (code::NOT_UTF8, DecodeError::NotUtf8.to_string()),
            Ok(Request::Hello { .. }) => (code::MALFORMED, "HELLO was already sent".to_string()),
            Err(e) => (code::MALFORMED, e.to_string()),
        };
        if let Next::Close = send_error(stream, failed.0, failed.1)? {
            return Ok(());
        }
    }
}

/// Runs one statement and makes its response.
fn run<F: Fs>(shared: &Shared<F>, sql: &str) -> Response {
    match oakdb_sql::execute(&shared.db, sql) {
        Ok(output) => Response::from(output),
        Err(e) => {
            let code = error_code(&e);
            if matches!(code, code::OUTCOME_UNKNOWN | code::STOPPED)
                && !shared.stop_logged.swap(true, Ordering::AcqRel)
            {
                eprintln!("oakdb: the engine stopped; restart the server: {e}");
            }
            Response::Error {
                code,
                message: e.to_string(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{read_frame, Response};

    // A panic on a connection's thread used to come from a planted disk
    // panic; since the commit thread turns its own panics into an engine
    // error (#27), nothing the tests can plant reaches this thread, so the
    // guard is tested directly.
    #[test]
    fn a_panic_while_serving_sends_0x7f() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, peer) = listener.accept().unwrap();
        serve_guarded(peer, &server, || {
            panic!("a bug in the server (planted by the test)")
        });
        drop(server);
        let frame = read_frame(&mut client)
            .unwrap()
            .expect("an ERROR before the close");
        match Response::decode(frame).unwrap() {
            Response::Error { code, .. } => assert_eq!(code, code::INTERNAL),
            other => panic!("expected ERROR, got {other:?}"),
        }
    }
}
