//! SIGINT and SIGTERM for `oakdb serve` (issue #38, design 0028 Q1).
//!
//! No signal handler: the two signals are blocked before any thread is
//! made, so every thread made after inherits the mask, the engine's commit
//! thread included, and one thread takes them with `sigwait` as a plain
//! return value. That thread may log, lock and connect, which a handler
//! may not (only async-signal-safe calls are allowed in one). The first
//! signal pulls the [`ShutdownHandle`]; one more ends the process at once
//! with 128 + the signal's number, as a shell reports a process a signal
//! killed (130 for SIGINT, 143 for SIGTERM).

use std::io;
use std::mem::MaybeUninit;
use std::thread::{self, JoinHandle};

use crate::server::ShutdownHandle;

/// SIGINT and SIGTERM, blocked in the thread that made this and in every
/// thread it makes from then on.
pub struct Signals {
    set: libc::sigset_t,
}

impl Signals {
    /// Blocks SIGINT and SIGTERM in the calling thread. Call it before any
    /// thread is made, the database's commit thread above all: a thread
    /// made earlier would keep the default action and could be the one a
    /// signal kills the process through.
    pub fn block() -> io::Result<Signals> {
        let mut set = MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: sigemptyset initialises the set it is given, and sigaddset
        // and pthread_sigmask read and change only the set and the calling
        // thread's mask; the pointers are to this local.
        let rc = unsafe {
            libc::sigemptyset(set.as_mut_ptr());
            libc::sigaddset(set.as_mut_ptr(), libc::SIGINT);
            libc::sigaddset(set.as_mut_ptr(), libc::SIGTERM);
            libc::pthread_sigmask(libc::SIG_BLOCK, set.as_ptr(), std::ptr::null_mut())
        };
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc));
        }
        // SAFETY: sigemptyset above initialised it.
        let set = unsafe { set.assume_init() };
        Ok(Signals { set })
    }

    /// Starts the thread that waits for the signals: the first starts the
    /// shutdown through `handle`, the next ends the process.
    pub fn spawn(self, handle: ShutdownHandle) -> io::Result<JoinHandle<()>> {
        thread::Builder::new()
            .name("oakdb signals".into())
            .stack_size(64 * 1024)
            .spawn(move || self.wait(&handle))
    }

    fn wait(&self, handle: &ShutdownHandle) {
        loop {
            let mut signal: libc::c_int = 0;
            // SAFETY: the set is initialised and both pointers are valid for
            // the call; sigwait writes the signal's number only.
            let rc = unsafe { libc::sigwait(&self.set, &mut signal) };
            if rc != 0 {
                continue;
            }
            let name = name(signal);
            if handle.requested() {
                eprintln!(
                    "oakdb: {name} again; stopping at once without the final checkpoint \
                     (every answered commit is in the WAL)"
                );
                std::process::exit(128 + signal);
            }
            eprintln!("oakdb: {name} received; shutting down (send it again to stop at once)");
            handle.shutdown();
        }
    }
}

fn name(signal: libc::c_int) -> &'static str {
    match signal {
        libc::SIGINT => "SIGINT",
        libc::SIGTERM => "SIGTERM",
        _ => "a signal",
    }
}
