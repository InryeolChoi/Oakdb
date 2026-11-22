//! Stub.
use crate::allow::Allowlist;
use oakdb::fs::Fs;
use oakdb::Db;
use std::io;
use std::net::SocketAddr;

pub const DEFAULT_PORT: u16 = 7878;
pub const DEFAULT_MAX_CONNECTIONS: usize = 512;

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub allow: Allowlist,
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
pub struct Server<F: Fs> {
    _db: Db<F>,
}
impl<F: Fs + 'static> Server<F>
where
    Db<F>: Send + Sync,
{
    pub fn bind(_db: Db<F>, _config: Config) -> io::Result<Self> {
        todo!()
    }
    pub fn local_addr(&self) -> SocketAddr {
        todo!()
    }
    pub fn run(self) {
        todo!()
    }
}
