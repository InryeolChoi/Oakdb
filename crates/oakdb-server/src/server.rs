//! The server: one thread per connection (ADR 0010, design 0020).
//!
//! The accepting thread checks each new client against the allowlist and
//! the connection limit, then hands it a thread of its own. That thread
//! reads the HELLO, answers READY, and then runs each QUERY through
//! `oakdb_sql::execute` (one statement, one transaction) and writes the
//! response, until the client leaves. Connections share the one database.

use std::io::{self, BufReader, Read};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use oakdb::fs::Fs;
use oakdb::Db;

use crate::allow::Allowlist;
use crate::protocol::{
    code, error_code, frame_len, read_frame, write_frame, DecodeError, FrameError, Request,
    Response, VERSION,
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
/// database's files, standard streams.
pub const FD_MARGIN: u64 = 64;

/// How long a refused or broken connection is given to take its error
/// before the socket closes.
const LINGER: Duration = Duration::from_secs(1);

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
    /// the port on the address the allowlist picks. Fails if the limit
    /// cannot be raised that far.
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
                refuse(stream, code::NOT_ALLOWED, message);
                continue;
            }
            let Some(slot) = Slot::take(&self.shared) else {
                let max = self.shared.max_connections;
                eprintln!("oakdb: refused {peer}: {max} connections already");
                refuse(
                    stream,
                    code::TOO_MANY_CONNECTIONS,
                    format!("too many connections (max {max})"),
                );
                continue;
            };
            let spawned = thread::Builder::new()
                .name(format!("oakdb {peer}"))
                .stack_size(CONNECTION_STACK)
                .spawn(move || {
                    if let Err(e) = serve(&slot.shared, stream) {
                        eprintln!("oakdb: connection from {peer}: {e}");
                    }
                    drop(slot);
                });
            // On failure the closure is dropped, which frees the slot and
            // closes the socket.
            if let Err(e) = spawned {
                eprintln!("oakdb: no thread for {peer}: {e}");
            }
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

/// Sends an error to a client turned away at the door, and closes. Runs
/// on the accepting thread, so it never waits: it reads off what the
/// client already sent (its HELLO) so closing does not reset the
/// connection before the error arrives, and leaves the rest.
fn refuse(stream: TcpStream, code: u8, message: String) {
    let _ = stream.set_write_timeout(Some(LINGER));
    let mut writer = &stream;
    let _ = write_frame(&mut writer, &Response::Error { code, message }.encode());
    let _ = stream.shutdown(Shutdown::Write);
    if stream.set_nonblocking(true).is_ok() {
        let mut sink = [0u8; 4096];
        while matches!((&stream).read(&mut sink), Ok(n) if n > 0) {}
    }
}

/// Sends a last error on a connection that cannot go on, and closes it
/// once the client has had a moment to read it.
fn close_with(stream: &TcpStream, code: u8, message: String) -> io::Result<()> {
    let mut writer = stream;
    write_frame(&mut writer, &Response::Error { code, message }.encode())?;
    linger(stream);
    Ok(())
}

/// Stops writing and reads what the client still sends, for up to
/// [`LINGER`], so the close does not reset the connection and throw away
/// the error it was just sent.
fn linger(stream: &TcpStream) {
    let _ = stream.shutdown(Shutdown::Write);
    let deadline = Instant::now() + LINGER;
    let _ = stream.set_read_timeout(Some(LINGER));
    let mut sink = [0u8; 64 * 1024];
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
        FrameError::Empty => close_with(stream, code::MALFORMED, e.to_string()),
        FrameError::TooLarge(_) => close_with(stream, code::FRAME_TOO_LARGE, e.to_string()),
        // The client left, mid-frame or with an error: nobody to tell.
        FrameError::Truncated => Ok(()),
        FrameError::Io(e) => Err(e),
    }
}

/// Runs one connection to its end.
fn serve<F: Fs>(shared: &Shared<F>, stream: TcpStream) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let mut reader = BufReader::new(&stream);
    let mut writer = &stream;

    // The handshake.
    let frame = match read_frame(&mut reader) {
        Ok(Some(frame)) => frame,
        Ok(None) => return Ok(()),
        Err(e) => return frame_failed(&stream, e),
    };
    match Request::decode(&frame) {
        Ok(Request::Hello { version: VERSION }) => {
            write_frame(&mut writer, &Response::Ready { version: VERSION }.encode())?;
        }
        Ok(Request::Hello { version }) => {
            let message =
                format!("the client speaks protocol {version}, this server speaks {VERSION}");
            return close_with(&stream, code::HANDSHAKE, message);
        }
        Err(DecodeError::BadMagic) => {
            return close_with(&stream, code::HANDSHAKE, "not an Oakdb client".into());
        }
        _ => {
            let message = "the first frame must be HELLO".to_string();
            return close_with(&stream, code::HANDSHAKE, message);
        }
    }

    loop {
        let frame = match read_frame(&mut reader) {
            Ok(Some(frame)) => frame,
            Ok(None) => return Ok(()),
            Err(e) => return frame_failed(&stream, e),
        };
        let sql = match Request::decode(&frame) {
            Ok(Request::Query(sql)) => sql,
            Err(DecodeError::NotUtf8) => {
                let not_utf8 = Response::Error {
                    code: code::NOT_UTF8,
                    message: DecodeError::NotUtf8.to_string(),
                };
                write_frame(&mut writer, &not_utf8.encode())?;
                continue;
            }
            Ok(Request::Hello { .. }) => {
                let message = "HELLO was already sent".to_string();
                return close_with(&stream, code::MALFORMED, message);
            }
            Err(e) => return close_with(&stream, code::MALFORMED, e.to_string()),
        };
        let response = match oakdb_sql::execute(&shared.db, &sql) {
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
        };
        let mut frame = response.encode();
        if frame_len(&frame).is_none() {
            let too_large = Response::Error {
                code: code::RESULT_TOO_LARGE,
                message: format!(
                    "the result is {} bytes, over the {} byte frame limit",
                    frame.payload.len(),
                    crate::protocol::MAX_FRAME_LEN
                ),
            };
            frame = too_large.encode();
        }
        write_frame(&mut writer, &frame)?;
    }
}
