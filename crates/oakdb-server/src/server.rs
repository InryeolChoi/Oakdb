//! The server: one thread per connection (ADR 0010, design 0020).
//!
//! The accepting thread checks each new client against the allowlist and
//! the connection limit, then hands it a thread of its own. That thread
//! reads the HELLO, answers READY, and then runs each QUERY on a
//! [`Session`] of its own and writes the response, until the client
//! leaves. Connections share the one database.
//!
//! The session lives on the connection's thread (issue #37, design 0025
//! Q1): outside BEGIN ... COMMIT each statement commits on its own, and
//! however the connection ends (the client closes, a frame breaks, the
//! thread panics, the client idles too long in a transaction) the session
//! is dropped and its open transaction rolled back. A connection that
//! waits longer than [`Config::idle_in_transaction_timeout`] for its next
//! statement inside a transaction gets [`code::IDLE_IN_TRANSACTION`] and
//! is closed (Q5).
//!
//! A [`ShutdownHandle`] stops the server gracefully (issue #38, design
//! 0028): the accepting thread is woken by a connection to itself, turns
//! away the clients still waiting at the door with
//! [`code::SHUTTING_DOWN`] and closes the listener. Each connection thread
//! looks at the flag while it waits for the first byte of its next frame,
//! every [`TICK`]: outside a transaction it closes with `0x49` at once,
//! inside one it goes on for [`Config::shutdown_grace`] and is then rolled
//! back and closed. A statement already running always finishes and is
//! answered first. Once every connection is gone the database is closed,
//! which is the final checkpoint. There is no list of connections, and no
//! socket is shut from another thread: that could reset the connection
//! and lose the `0x49` (Q5).

use std::io::{self, BufReader, Read};
use std::net::{self, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::thread;
use std::time::{Duration, Instant};

use oakdb::fs::Fs;
use oakdb::{Db, Stats};
use oakdb_sql::Session;

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

/// How long a connection may wait for its next statement inside a
/// transaction unless told otherwise (design 0025 Q5).
pub const DEFAULT_IDLE_IN_TRANSACTION_TIMEOUT: Duration = Duration::from_secs(60);

/// How old a transaction gets before the server logs a warning about it,
/// unless told otherwise (design 0025 Q7).
pub const DEFAULT_LONG_TRANSACTION_WARNING: Duration = Duration::from_secs(5 * 60);

/// How long transactions get to finish once shutdown starts, unless told
/// otherwise (design 0028 Q4): with the margin and the checkpoint it fits
/// in Kubernetes' default 30 seconds.
pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// How much longer than the grace the server waits for statements still
/// running before it gives up and ends without the final checkpoint
/// (design 0028 Q4). Only a stuck sync or a very long scan gets here.
pub const SHUTDOWN_MARGIN: Duration = Duration::from_secs(5);

/// How often a connection waiting for the first byte of its next frame
/// wakes to look at the shutdown flag (design 0028 Q5).
pub const TICK: Duration = Duration::from_millis(100);

/// The message that goes with [`code::SHUTTING_DOWN`].
pub const SHUTTING_DOWN_MESSAGE: &str = "server is shutting down";

/// How long the handle keeps trying to wake the accepting thread.
const WAKE_FOR: Duration = Duration::from_secs(10);

/// How long a refused or broken connection is given to take its error
/// before the socket closes.
const LINGER: Duration = Duration::from_secs(1);

/// Refused connections that may linger at once, each on a small thread of
/// its own. Past this a refusal closes at once, as before (review P3).
const MAX_LINGERING: usize = 32;

/// Stack of a thread that only lingers on a refused connection.
const LINGER_STACK: usize = 64 * 1024;

/// How the server runs. Fields are added from ticket to ticket, so it is
/// built from [`Config::default`]: `Config::default().with(|c| c.port = 0)`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Config {
    /// TCP port; 0 lets the operating system pick one.
    pub port: u16,
    /// Who may connect. It also picks the address to listen on.
    pub allow: Allowlist,
    /// Connections taken at once. One more is refused with
    /// [`code::TOO_MANY_CONNECTIONS`].
    pub max_connections: usize,
    /// How long a connection inside a transaction may wait for its next
    /// statement, or sit halfway through sending one, before the server
    /// rolls the transaction back and closes it with
    /// [`code::IDLE_IN_TRANSACTION`]. Writing a response to it is held to
    /// the same limit. `None` or zero: no limit.
    pub idle_in_transaction_timeout: Option<Duration>,
    /// The same outside a transaction, closing with [`code::IDLE`]. Off by
    /// default: such a connection holds a seat and a small stack but
    /// nothing of the engine's, and pools keep connections open on purpose.
    pub idle_timeout: Option<Duration>,
    /// A transaction this old gets one warning in the log, saying it holds
    /// old pages and conflict records. `None` or zero: no warning.
    pub long_transaction_warning: Option<Duration>,
    /// Once shutdown starts, how long a connection inside a transaction may
    /// go on to finish it before the server rolls it back and closes it
    /// with [`code::SHUTTING_DOWN`]. Zero rolls them back at once.
    pub shutdown_grace: Duration,
}

impl Config {
    /// This configuration with `change` made to it.
    pub fn with(mut self, change: impl FnOnce(&mut Config)) -> Config {
        change(&mut self);
        self
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            port: DEFAULT_PORT,
            allow: Allowlist::default(),
            max_connections: DEFAULT_MAX_CONNECTIONS,
            idle_in_transaction_timeout: Some(DEFAULT_IDLE_IN_TRANSACTION_TIMEOUT),
            idle_timeout: None,
            long_transaction_warning: Some(DEFAULT_LONG_TRANSACTION_WARNING),
            shutdown_grace: DEFAULT_SHUTDOWN_GRACE,
        }
    }
}

/// `None` for a limit of zero, which means none.
fn limit(d: Option<Duration>) -> Option<Duration> {
    d.filter(|d| !d.is_zero())
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
    idle_in_transaction: Option<Duration>,
    idle: Option<Duration>,
    long_transaction: Option<Duration>,
    /// The shutdown flag, shared with every [`ShutdownHandle`].
    stop: Arc<Stop>,
    grace: Duration,
    /// Connections closed with `0x49`, and the transactions rolled back
    /// with them.
    closed: AtomicUsize,
    rolled_back: AtomicUsize,
}

impl<F: Fs> Shared<F> {
    /// Whether a connection should now be closed with `0x49`: shutdown has
    /// started, and it is outside a transaction or the grace is over.
    fn shutting_down(&self, in_transaction: bool) -> bool {
        match self.stop.at.get() {
            None => false,
            Some(at) => !in_transaction || at.elapsed() >= self.grace,
        }
    }
}

/// Reads a running server's numbers from any thread, for operators and
/// tests (design 0025 Q7). It does not keep the database open: once the
/// server has shut down it reads zeros.
pub struct Monitor<F: Fs> {
    shared: Weak<Shared<F>>,
}

impl<F: Fs> Clone for Monitor<F> {
    fn clone(&self) -> Self {
        Monitor {
            shared: Weak::clone(&self.shared),
        }
    }
}

impl<F: Fs> Monitor<F> {
    /// The engine's numbers: commits, conflicts, open transactions and the
    /// commits kept for them.
    pub fn stats(&self) -> Stats {
        self.shared
            .upgrade()
            .map(|shared| shared.db.stats())
            .unwrap_or_default()
    }

    /// Connections being served now, each on its own thread.
    pub fn connections(&self) -> usize {
        self.shared
            .upgrade()
            .map_or(0, |shared| shared.active.load(Ordering::Acquire))
    }
}

/// When shutdown started, and where to knock to wake the accepting thread.
#[derive(Debug, Default)]
struct Stop {
    /// Set once, by the first [`ShutdownHandle::shutdown`].
    at: OnceLock<Instant>,
    /// The listener's address, once the server is bound.
    wake: Mutex<Option<SocketAddr>>,
    /// Set once the accepting thread has stopped accepting.
    door_closed: AtomicBool,
}

/// Starts a server's graceful shutdown from any thread: the signal thread
/// of `oakdb serve`, or a test (design 0028 Q1). Clones pull the same
/// handle; pulling it again does nothing.
#[derive(Debug, Clone, Default)]
pub struct ShutdownHandle {
    stop: Arc<Stop>,
}

impl ShutdownHandle {
    /// A handle for a server not bound yet; [`Server::bind_with`] takes it.
    /// Pulled before the server runs, `run` shuts down at once.
    pub fn new() -> Self {
        ShutdownHandle::default()
    }

    /// Starts the shutdown and returns at once; [`Server::run`] returns
    /// once it is over.
    pub fn shutdown(&self) {
        if self.stop.at.set(Instant::now()).is_err() {
            return;
        }
        let addr = *self
            .stop
            .wake
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(addr) = addr {
            let stop = Arc::clone(&self.stop);
            let spawned = thread::Builder::new()
                .name("oakdb wake".into())
                .stack_size(LINGER_STACK)
                .spawn(move || wake(&stop, addr));
            if spawned.is_err() {
                let _ = TcpStream::connect_timeout(&addr, LINGER);
            }
        }
    }

    /// Whether the shutdown has started.
    pub fn requested(&self) -> bool {
        self.stop.at.get().is_some()
    }
}

/// Wakes the accepting thread, asleep in `accept`, by connecting to it
/// (design 0028 Q2). It keeps knocking until the door is closed: a knock
/// can fail when descriptors run out, and then `accept` fails too and the
/// accepting thread sees the flag by itself.
fn wake(stop: &Stop, addr: SocketAddr) {
    let give_up = Instant::now() + WAKE_FOR;
    while !stop.door_closed.load(Ordering::Acquire) && Instant::now() < give_up {
        match TcpStream::connect_timeout(&addr, LINGER) {
            Ok(_knock) => {
                let waited = Instant::now() + LINGER;
                while !stop.door_closed.load(Ordering::Acquire) && Instant::now() < waited {
                    thread::sleep(Duration::from_millis(5));
                }
            }
            // The listener is gone: nobody left to wake.
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => return,
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// What a shutdown came to, for the log and the exit code (design 0028
/// Q8).
#[derive(Debug)]
#[non_exhaustive]
pub struct Shutdown {
    /// Connections being served when it started.
    pub connections: usize,
    /// Transactions open when it started.
    pub in_transaction: u64,
    /// Connections closed with [`code::SHUTTING_DOWN`].
    pub closed: usize,
    /// Transactions rolled back because the grace ran out.
    pub rolled_back: usize,
    /// Connections still running a statement past the grace and
    /// [`SHUTDOWN_MARGIN`]. If any, the database was not closed.
    pub busy: usize,
    /// What the final checkpoint gave: `None` if the database could not be
    /// closed because connections were still busy.
    pub checkpoint: Option<oakdb::Result<()>>,
    /// From the start of the shutdown to its end.
    pub took: Duration,
}

impl Shutdown {
    /// Whether it ended as designed: every connection gone and the final
    /// checkpoint done. `oakdb serve` exits 0 then, 1 otherwise.
    pub fn is_clean(&self) -> bool {
        self.busy == 0 && matches!(self.checkpoint, Some(Ok(())))
    }
}

impl<F: Fs + 'static> Server<F>
where
    Db<F>: Send + Sync,
{
    /// Raises the file descriptor limit to fit `max_connections`, and binds
    /// the port on the address the allowlist picks. Fails, saying what to
    /// do, if the limit cannot be raised that far.
    pub fn bind(db: Db<F>, config: Config) -> io::Result<Self> {
        Self::bind_with(db, config, ShutdownHandle::new())
    }

    /// [`Server::bind`], with a shutdown handle made beforehand, so that a
    /// signal that comes while the database opens is not lost.
    pub fn bind_with(db: Db<F>, config: Config, handle: ShutdownHandle) -> io::Result<Self> {
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
        let mut wake = listener.local_addr()?;
        if wake.ip().is_unspecified() {
            // Listening on every address: knock on loopback, which the
            // allowlist always lets in.
            wake.set_ip(Ipv4Addr::LOCALHOST.into());
        }
        *handle
            .stop
            .wake
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(wake);
        Ok(Server {
            listener,
            shared: Arc::new(Shared {
                db,
                allow: config.allow,
                max_connections: config.max_connections,
                active: AtomicUsize::new(0),
                stop_logged: AtomicBool::new(false),
                idle_in_transaction: limit(config.idle_in_transaction_timeout),
                idle: limit(config.idle_timeout),
                long_transaction: limit(config.long_transaction_warning),
                stop: handle.stop,
                grace: config.shutdown_grace,
                closed: AtomicUsize::new(0),
                rolled_back: AtomicUsize::new(0),
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

    /// A handle that reads this server's numbers while it runs.
    pub fn monitor(&self) -> Monitor<F> {
        Monitor {
            shared: Arc::downgrade(&self.shared),
        }
    }

    /// A handle that shuts this server down.
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle {
            stop: Arc::clone(&self.shared.stop),
        }
    }

    /// Takes connections until the shutdown handle is pulled, then shuts
    /// down (design 0028) and says how that went.
    pub fn run(self) -> Shutdown {
        let Server {
            listener,
            shared,
            lingering,
        } = self;
        accept(&listener, &shared, &lingering);
        shared.stop.door_closed.store(true, Ordering::Release);
        let at = *shared
            .stop
            .at
            .get()
            .expect("accepting stops only on shutdown");
        let connections = shared.active.load(Ordering::Acquire);
        let in_transaction = shared.db.stats().open_transactions;
        eprintln!(
            "oakdb: shutting down: {connections} connections, {in_transaction} in a transaction; \
             waiting up to {}s for transactions",
            shared.grace.as_secs_f64()
        );
        let turned_away = turn_away_waiting(&listener, &lingering);
        // New clients are refused by the system from here on.
        drop(listener);
        if turned_away > 0 {
            eprintln!("oakdb: turned away {turned_away} connections waiting at the door");
        }

        let give_up = at + shared.grace + SHUTDOWN_MARGIN;
        while shared.active.load(Ordering::Acquire) > 0 && Instant::now() < give_up {
            thread::sleep(Duration::from_millis(5));
        }
        let closed = shared.closed.load(Ordering::Acquire);
        let rolled_back = shared.rolled_back.load(Ordering::Acquire);
        if rolled_back > 0 {
            eprintln!("oakdb: rolled back {rolled_back} open transactions");
        }
        let busy = shared.active.load(Ordering::Acquire);
        let mut report = Shutdown {
            connections,
            in_transaction,
            closed,
            rolled_back,
            busy,
            checkpoint: None,
            took: Duration::ZERO,
        };
        if busy > 0 {
            eprintln!(
                "oakdb: {busy} connections still running a statement; ending without the final \
                 checkpoint, and the next start replays the WAL"
            );
            report.took = at.elapsed();
            return report;
        }
        // A connection thread lets go of the database a moment after it
        // gives its seat back; a monitor holds it for a moment too.
        let mut shared = shared;
        let db = loop {
            match Arc::try_unwrap(shared) {
                Ok(only) => break Some(only.db),
                Err(back) if Instant::now() < give_up => {
                    shared = back;
                    thread::sleep(Duration::from_millis(1));
                }
                Err(_) => break None,
            }
        };
        report.checkpoint = db.map(|db| db.close());
        match &report.checkpoint {
            Some(Ok(())) => eprintln!("oakdb: checkpointed and closed"),
            Some(Err(e)) => eprintln!(
                "oakdb: the final checkpoint failed: {e}; the next start replays the WAL, \
                 and every answered commit is in it"
            ),
            None => eprintln!(
                "oakdb: the database is still in use; ending without the final checkpoint"
            ),
        }
        report.took = at.elapsed();
        report
    }
}

/// Takes connections until the shutdown starts. Each one gets the flag
/// looked at first, so the connection that woke this thread takes no seat.
fn accept<F: Fs + 'static>(
    listener: &TcpListener,
    shared: &Arc<Shared<F>>,
    lingering: &Arc<AtomicUsize>,
) where
    Db<F>: Send + Sync,
{
    while shared.stop.at.get().is_none() {
        let (stream, peer) = match listener.accept() {
            Ok(conn) => conn,
            Err(e) => {
                if shared.stop.at.get().is_some() {
                    break;
                }
                // Out of descriptors and the like: wait instead of
                // spinning.
                eprintln!("oakdb: accepting a connection failed: {e}");
                thread::sleep(Duration::from_millis(10));
                continue;
            }
        };
        if shared.stop.at.get().is_some() {
            refuse(
                lingering,
                stream,
                code::SHUTTING_DOWN,
                SHUTTING_DOWN_MESSAGE.into(),
            );
            break;
        }
        if !shared.allow.allows(peer.ip()) {
            eprintln!("oakdb: refused {peer}: not on the allowlist");
            let message = format!("address {} is not allowed", peer.ip());
            refuse(lingering, stream, code::NOT_ALLOWED, message);
            continue;
        }
        let Some(slot) = Slot::take(shared) else {
            let max = shared.max_connections;
            eprintln!("oakdb: refused {peer}: {max} connections already");
            let message = format!("too many connections (max {max})");
            refuse(lingering, stream, code::TOO_MANY_CONNECTIONS, message);
            continue;
        };
        let spawned = thread::Builder::new()
            .name(format!("oakdb {peer}"))
            .stack_size(CONNECTION_STACK)
            .spawn(move || {
                eprintln!("oakdb: {peer} connected");
                serve_guarded(peer, &stream, || serve(&slot.shared, &stream, peer));
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

/// Sends `0x49` to every client already waiting in the system's queue, so
/// they learn why instead of seeing the connection reset when the listener
/// closes. Returns how many there were.
fn turn_away_waiting(listener: &TcpListener, lingering: &Arc<AtomicUsize>) -> usize {
    if listener.set_nonblocking(true).is_err() {
        return 0;
    }
    let mut count = 0;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                // On macOS a socket accepted from a nonblocking listener is
                // nonblocking too, and lingering on it would not wait
                // (design 0028 Q9). Linux does not pass it on.
                let _ = stream.set_nonblocking(false);
                refuse(
                    lingering,
                    stream,
                    code::SHUTTING_DOWN,
                    SHUTTING_DOWN_MESSAGE.into(),
                );
                count += 1;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // `WouldBlock`: nobody else is waiting.
            Err(_) => return count,
        }
    }
}

/// Sends an error to a client turned away at the door, and closes once
/// it has had a moment to read it. The waiting happens on a small thread
/// of its own, so the accepting thread never waits; past
/// [`MAX_LINGERING`] of them the socket closes at once, after reading off
/// what has already arrived.
fn refuse(lingering: &Arc<AtomicUsize>, stream: TcpStream, code: u8, message: String) {
    let _ = stream.set_write_timeout(Some(LINGER));
    let mut writer = &stream;
    let _ = write_frame(&mut writer, &error_frame(code, message));
    let _ = stream.shutdown(net::Shutdown::Write);
    if let Some(turn) = Lingering::take(lingering) {
        // If no thread can be had, the closure is dropped and the socket
        // closes at once.
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

/// Sends an `ERROR`. After a code that [`closes`] the connection, it waits
/// for the client to read it and says to close.
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
    let _ = stream.shutdown(net::Shutdown::Write);
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

/// Whether a failed read or write gave up on its timeout. macOS and Linux
/// say `WouldBlock`; other systems may say `TimedOut` (design 0025 Q5).
fn timed_out(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// The socket's read and write timeouts, set only when they change. The
/// wait for a frame's first byte reads in [`TICK`]s and the rest of the
/// frame under the idle limit, so a statement costs two calls to set the
/// read timeout; the write timeout is set only when the limit changes.
struct Timeouts<'s> {
    stream: &'s TcpStream,
    read: Option<Duration>,
    write: Option<Duration>,
}

impl Timeouts<'_> {
    fn read(&mut self, t: Option<Duration>) -> io::Result<()> {
        if t != self.read {
            self.stream.set_read_timeout(t)?;
            self.read = t;
        }
        Ok(())
    }

    fn write(&mut self, t: Option<Duration>) -> io::Result<()> {
        if t != self.write {
            self.stream.set_write_timeout(t)?;
            self.write = t;
        }
        Ok(())
    }
}

/// When the open transaction began, and whether its age was logged.
struct Open {
    since: Instant,
    warned: bool,
}

/// What waiting for the next frame came to.
enum Wait {
    /// Its first byte is here.
    Ready,
    /// The client closed the connection between frames.
    Closed,
    /// The idle limit passed first.
    Idle,
    /// The server is shutting down, and this connection is to close now.
    ShuttingDown,
}

/// Runs one connection to its end.
fn serve<F: Fs>(shared: &Shared<F>, stream: &TcpStream, peer: SocketAddr) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let mut reader = BufReader::new(stream);
    let mut timeouts = Timeouts {
        stream,
        read: None,
        write: None,
    };

    // The handshake, waited for as any frame is, so a shutdown reaches a
    // client that has not said HELLO yet too.
    match wait_for_frame(
        shared,
        &mut reader,
        &mut timeouts,
        None,
        &mut None,
        peer,
        false,
    )? {
        Wait::Ready => {}
        Wait::Closed => return Ok(()),
        Wait::Idle | Wait::ShuttingDown => return shut(shared, stream, None),
    }
    timeouts.read(None)?;
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
    if shared.shutting_down(false) {
        return shut(shared, stream, None);
    }
    send(stream, &Response::Ready { version: VERSION })?;

    // Dropped however this function ends, unwinding included: that rolls
    // an open transaction back (design 0025 Q1).
    let mut session = Session::new(&shared.db);
    let mut open: Option<Open> = None;
    loop {
        let in_transaction = session.in_transaction();
        let idle = if in_transaction {
            shared.idle_in_transaction
        } else {
            shared.idle
        };
        if let Err(e) = timeouts.write(idle) {
            return Err(rolled_back_on_error(session, e));
        }
        let waited = wait_for_frame(
            shared,
            &mut reader,
            &mut timeouts,
            idle,
            &mut open,
            peer,
            in_transaction,
        )?;
        match waited {
            Wait::Ready => {}
            Wait::Closed => return Ok(()),
            Wait::Idle => return idled(stream, session, idle, peer),
            Wait::ShuttingDown => return shut(shared, stream, Some(session)),
        }
        // Halfway through a frame, each read gets the same limit.
        timeouts.read(idle)?;
        let frame = match read_frame(&mut reader) {
            Ok(Some(frame)) => frame,
            Ok(None) => return Ok(()),
            Err(FrameError::Io(e)) if timed_out(&e) => {
                return idled(stream, session, idle, peer);
            }
            Err(e) => return frame_failed(stream, e),
        };
        let failed = match Request::decode(frame) {
            Ok(Request::Query(sql)) => {
                let response = run(shared, &mut session, &sql);
                open = match (open, session.in_transaction()) {
                    (None, true) => Some(Open {
                        since: Instant::now(),
                        warned: false,
                    }),
                    (open, true) => open,
                    (_, false) => None,
                };
                warn_if_old(shared, &mut open, peer);
                if let Err(e) = send(stream, &response) {
                    return Err(rolled_back_on_error(session, e));
                }
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

/// Waits for the first byte of the next frame, at most `idle`. Only this
/// wait counts as idle: running a statement, its commit's sync included,
/// does not (design 0025 Q5). It wakes early to log the open
/// transaction's age once that passes the warning limit, and every
/// [`TICK`] to look at the shutdown flag (design 0028 Q5), and waits on;
/// `fill_buf` takes no byte, so waking loses nothing. The flag is looked
/// at before anything already sent is read: once a connection is to
/// close, a statement sent after its last answer is not run.
#[allow(clippy::too_many_arguments)]
fn wait_for_frame<F: Fs>(
    shared: &Shared<F>,
    reader: &mut BufReader<&TcpStream>,
    timeouts: &mut Timeouts<'_>,
    idle: Option<Duration>,
    open: &mut Option<Open>,
    peer: SocketAddr,
    in_transaction: bool,
) -> io::Result<Wait> {
    use std::io::BufRead;
    let started = Instant::now();
    let deadline = idle.map(|idle| started + idle);
    loop {
        if shared.shutting_down(in_transaction) {
            return Ok(Wait::ShuttingDown);
        }
        let warn_at = match (&*open, shared.long_transaction) {
            (Some(o), Some(after)) if !o.warned => Some(o.since + after),
            _ => None,
        };
        let wake = match (deadline, warn_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        // A timeout of zero is refused, so at least a millisecond. With
        // nothing due before the next tick the timeout stays TICK, so
        // waiting on sets no option again.
        let now = Instant::now();
        let timeout = match wake {
            Some(w) if w < now + TICK => w
                .saturating_duration_since(now)
                .max(Duration::from_millis(1)),
            _ => TICK,
        };
        timeouts.read(Some(timeout))?;
        match reader.fill_buf() {
            Ok([]) => return Ok(Wait::Closed),
            Ok(_) => return Ok(Wait::Ready),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if timed_out(&e) => {
                warn_if_old(shared, open, peer);
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    return Ok(Wait::Idle);
                }
            }
            Err(e) => return Err(e),
        }
    }
}

/// Logs, once, that the open transaction has passed the warning age.
fn warn_if_old<F: Fs>(shared: &Shared<F>, open: &mut Option<Open>, peer: SocketAddr) {
    let (Some(o), Some(after)) = (open.as_mut(), shared.long_transaction) else {
        return;
    };
    let age = o.since.elapsed();
    if !o.warned && age >= after {
        o.warned = true;
        let stats = shared.db.stats();
        eprintln!(
            "oakdb: warning: {peer} has had a transaction open for {}s; it holds old pages \
             in memory and {} commits for conflict checks ({} transactions open)",
            age.as_secs(),
            stats.kept_commits,
            stats.open_transactions,
        );
    }
}

/// Ends `session` for a connection that failed on a write, and says in
/// the error, which [`serve_guarded`] logs, whether that rolled a
/// transaction back: a client that stops reading a large result is cut off
/// by the write timeout, and the operator should see what it lost
/// (review S5).
fn rolled_back_on_error<F: Fs>(session: Session<'_, F>, e: io::Error) -> io::Error {
    if !session.in_transaction() {
        return e;
    }
    drop(session);
    let what = if timed_out(&e) {
        "sending timed out".to_string()
    } else {
        e.to_string()
    };
    io::Error::new(
        e.kind(),
        format!("{what}; the open transaction was rolled back and the connection closed"),
    )
}

/// The connection idled past its limit: rolls back what it held, tells it
/// so, and closes.
fn idled<F: Fs>(
    stream: &TcpStream,
    session: Session<'_, F>,
    idle: Option<Duration>,
    peer: SocketAddr,
) -> io::Result<()> {
    let secs = idle.unwrap_or_default().as_secs_f64();
    let (code, message) = if session.in_transaction() {
        // Rolled back before the client hears of it, so whatever it does
        // next finds the transaction gone.
        drop(session);
        eprintln!("oakdb: {peer} idle in a transaction for {secs}s; rolled back and closed");
        (
            code::IDLE_IN_TRANSACTION,
            format!(
                "idle in a transaction for more than {secs}s; the transaction was rolled back \
                 and the connection closed"
            ),
        )
    } else {
        eprintln!("oakdb: {peer} idle for {secs}s; closed");
        (
            code::IDLE,
            format!("idle for more than {secs}s; the connection closed"),
        )
    };
    // Whatever half-frame was read is dropped with the connection.
    let _ = stream.set_write_timeout(Some(LINGER));
    send_error(stream, code, message).map(|_| ())
}

/// The server is shutting down: rolls back what the connection held, tells
/// it so with `0x49`, and closes (design 0028 Q3, Q6). Every statement it
/// ran has been answered already.
fn shut<F: Fs>(
    shared: &Shared<F>,
    stream: &TcpStream,
    session: Option<Session<'_, F>>,
) -> io::Result<()> {
    if session.as_ref().is_some_and(Session::in_transaction) {
        shared.rolled_back.fetch_add(1, Ordering::AcqRel);
    }
    // Rolled back before the client hears of it.
    drop(session);
    shared.closed.fetch_add(1, Ordering::AcqRel);
    let _ = stream.set_write_timeout(Some(LINGER));
    send_error(stream, code::SHUTTING_DOWN, SHUTTING_DOWN_MESSAGE.into()).map(|_| ())
}

/// Runs one statement on the connection's session and makes its response.
fn run<F: Fs>(shared: &Shared<F>, session: &mut Session<'_, F>, sql: &str) -> Response {
    #[cfg(test)]
    if sql == tests::PANIC {
        panic!("a bug in the server (planted by the test)");
    }
    match session.execute(sql) {
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

    /// A statement [`run`] panics on, in test builds only: no statement
    /// reaches a real panic on a connection's thread (design 0025 Q8).
    pub(super) const PANIC: &str = "-- panic, planted by the test";

    #[test]
    fn a_panic_inside_a_transaction_sends_0x7f_and_rolls_the_transaction_back() {
        use crate::protocol::write_frame;
        use oakdb::fs::sim::SimFs;

        let shared = Arc::new(Shared {
            db: Db::open_with(SimFs::new(), "/oak").unwrap(),
            allow: Allowlist::default(),
            max_connections: 1,
            active: AtomicUsize::new(0),
            stop_logged: AtomicBool::new(false),
            idle_in_transaction: None,
            idle: None,
            long_transaction: None,
            stop: Arc::default(),
            grace: Duration::ZERO,
            closed: AtomicUsize::new(0),
            rolled_back: AtomicUsize::new(0),
        });
        oakdb_sql::execute(&shared.db, "CREATE TABLE t (id INTEGER PRIMARY KEY)").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let (server, peer) = listener.accept().unwrap();
        let serving = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || serve_guarded(peer, &server, || serve(&shared, &server, peer)))
        };

        let mut ask = |request: Request| {
            write_frame(&mut client, &request.encode()).unwrap();
            Response::decode(read_frame(&mut client).unwrap().unwrap()).unwrap()
        };
        ask(Request::Hello { version: VERSION });
        assert_eq!(ask(Request::Query("BEGIN".into())), Response::Done);
        assert_eq!(
            ask(Request::Query("INSERT INTO t VALUES (1)".into())),
            Response::Affected(1)
        );
        assert_eq!(shared.db.stats().open_transactions, 1);
        match ask(Request::Query(PANIC.into())) {
            Response::Error { code, .. } => assert_eq!(code, code::INTERNAL),
            other => panic!("expected ERROR, got {other:?}"),
        }
        serving.join().unwrap();

        // Unwinding dropped the session, and with it the transaction.
        assert_eq!(shared.db.stats().open_transactions, 0);
        let out = oakdb_sql::execute(&shared.db, "SELECT id FROM t").unwrap();
        assert_eq!(
            out,
            oakdb_sql::Output::Rows {
                columns: vec!["id".into()],
                rows: vec![]
            }
        );
    }

    #[test]
    fn the_last_connection_ending_just_before_the_margin_still_gets_the_checkpoint() {
        // Review S2: the database is let go of a moment after the last seat
        // is given back. When that happens at the very end of the grace and
        // margin, the close still needs a moment of its own.
        use oakdb::fs::sim::SimFs;

        let db = Db::open_with(SimFs::new(), "/oak").unwrap();
        let config = Config::default().with(|c| {
            c.port = 0;
            c.shutdown_grace = Duration::ZERO;
        });
        let server = Server::bind(db, config).unwrap();
        // The shutdown started a grace and a margin ago: the deadline is
        // now, and nobody is seated.
        let started = Instant::now()
            .checked_sub(SHUTDOWN_MARGIN)
            .expect("the clock goes back that far");
        server.shared.stop.at.set(started).unwrap();
        // Held as a connection thread holds it after giving its seat back.
        let held = Arc::clone(&server.shared);
        let letting_go = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            drop(held);
        });
        let report = server.run();
        letting_go.join().unwrap();
        assert!(report.is_clean(), "{report:?}");
    }

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
