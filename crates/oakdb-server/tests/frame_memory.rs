//! A frame's length is only a claim (review S2): memory must grow with the
//! bytes that arrive, not with the length a client writes. This file counts
//! every allocation of its process, so it holds a single test, run alone.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use oakdb::fs::sim::SimFs;
use oakdb::Db;
use oakdb_server::protocol::{
    command, read_frame, write_frame, FrameError, Request, Response, MAX_FRAME_LEN, VERSION,
};
use oakdb_server::server::{Config, Server};

/// The system allocator, counting the bytes allocated now and at most.
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(by: usize) {
    let now = LIVE.fetch_add(by, Ordering::SeqCst) + by;
    PEAK.fetch_max(now, Ordering::SeqCst);
}

// SAFETY: every call goes to the system allocator unchanged; the counters
// only watch.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        grew(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        grew(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > layout.size() {
            grew(new_size - layout.size());
        } else {
            LIVE.fetch_sub(layout.size() - new_size, Ordering::SeqCst);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const MIB: usize = 1024 * 1024;

#[test]
fn a_claimed_length_does_not_reserve_memory_until_bytes_arrive() {
    // One read: a header claiming the limit, then a few bytes and the end.
    let mut bytes = MAX_FRAME_LEN.to_be_bytes().to_vec();
    bytes.push(command::QUERY);
    bytes.extend_from_slice(b"SELECT");
    let before = LIVE.load(Ordering::SeqCst);
    PEAK.store(before, Ordering::SeqCst);
    assert!(matches!(
        read_frame(&mut bytes.as_slice()),
        Err(FrameError::Truncated)
    ));
    let peak = PEAK.load(Ordering::SeqCst) - before;
    assert!(peak < MIB, "reading 11 bytes reserved {peak} bytes");

    // A server: connections each claim 16 MiB and send nothing more. With
    // the length trusted they would hold 16 MiB each, 512 MiB in all.
    const CLIENTS: usize = 32;
    let server = Server::bind(
        Db::open_with(SimFs::new(), "/db").unwrap(),
        Config {
            port: 0,
            ..Config::default()
        },
    )
    .unwrap();
    let addr = server.local_addr();
    thread::spawn(move || server.run());

    let before = LIVE.load(Ordering::SeqCst);
    let streams: Vec<TcpStream> = (0..CLIENTS)
        .map(|_| {
            let mut s = TcpStream::connect(addr).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
            write_frame(&mut s, &Request::Hello { version: VERSION }.encode()).unwrap();
            let ready = read_frame(&mut s).unwrap().unwrap();
            assert_eq!(
                Response::decode(ready).unwrap(),
                Response::Ready { version: VERSION }
            );
            let mut claim = MAX_FRAME_LEN.to_be_bytes().to_vec();
            claim.push(command::QUERY);
            s.write_all(&claim).unwrap();
            s
        })
        .collect();
    // Give every connection thread time to read its header and wait.
    thread::sleep(Duration::from_millis(500));
    let held = LIVE.load(Ordering::SeqCst).saturating_sub(before);
    assert!(
        held < 16 * MIB,
        "{CLIENTS} connections that sent a header hold {} MiB",
        held / MIB
    );
    drop(streams);
}
