//! The limit on open file descriptors (design 0020, section 1-1).
//!
//! Every connection is a socket, and a socket is a file descriptor. macOS
//! starts processes with a soft limit of 256, so a server taking 512
//! connections must raise it. The soft limit can go up to the hard limit
//! without privileges.

use std::io;

/// Highest soft limit macOS's `setrlimit` takes for open files
/// (`OPEN_MAX`), whatever the hard limit says.
#[cfg(target_os = "macos")]
const MACOS_OPEN_MAX: u64 = 10240;

/// Raises the soft limit on open files to at least `want`, as far as the
/// hard limit allows, and returns the soft limit it ends at. Never lowers
/// it.
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
    if current >= want {
        return Ok(current);
    }
    #[allow(unused_mut)]
    let mut target = want.min(limit.rlim_max);
    #[cfg(target_os = "macos")]
    {
        target = target.min(MACOS_OPEN_MAX);
    }
    if target <= current {
        return Ok(current);
    }
    limit.rlim_cur = target;
    // SAFETY: setrlimit reads one rlimit through a valid pointer.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(target)
}
