//! The Oakdb server (issue #36, design 0020): the wire protocol, the
//! server that runs SQL for clients over TCP, and the CLI client.

pub mod allow;
pub mod cli;
pub mod client;
pub mod protocol;
#[doc(hidden)]
pub mod rlimit;
pub mod server;
#[cfg(unix)]
pub mod signal;
