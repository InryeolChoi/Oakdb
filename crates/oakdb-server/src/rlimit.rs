//! The limit on open file descriptors (design 0020, section 1-1).
//!
//! Every connection is a socket, and a socket is a file descriptor. macOS
//! starts processes with a soft limit of 256, so a server taking 512
//! connections must raise it. The soft limit can go up to the hard limit
//! without privileges.

use std::io;

// rlim_t is u64 on the 64-bit macOS and Linux that ADR 0003 names; this
// says so where the build would break if it were not (review S5).
const _: () = assert!(std::mem::size_of::<libc::rlim_t>() == 8);

/// Highest soft limit macOS takes for open files whatever the hard limit
/// says (`OPEN_MAX`). Linux has no such cap.
#[cfg(target_os = "macos")]
const CAP: u64 = 10240;

/// Raises the soft limit on open files to at least `want`, as far as the
/// hard limit allows, and returns the soft limit it ends at. Never lowers
/// it. If the system refuses the raise (macOS's `kern.maxfilesperproc` can
/// sit below `OPEN_MAX`), it keeps the limit as it was and returns that, so
/// the caller can say what to do (review S4, P6).
pub fn raise_fd_limit(want: u64) -> io::Result<u64> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes one rlimit through a valid pointer.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let current: u64 = limit.rlim_cur;
    let target = want.min(limit.rlim_max);
    #[cfg(target_os = "macos")]
    let target = target.min(CAP);
    if target <= current {
        return Ok(current);
    }
    limit.rlim_cur = target;
    // SAFETY: setrlimit reads one rlimit through a valid pointer.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } != 0 {
        return Ok(current);
    }
    Ok(target)
}
