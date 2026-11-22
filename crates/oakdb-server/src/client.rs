//! Stub.
use oakdb_sql::Output;
use std::io;
use std::net::ToSocketAddrs;
use std::time::Duration;

#[derive(Debug)]
pub enum ClientError {
    Io(io::Error),
    Protocol(String),
    Server { code: u8, message: String },
    Closed,
}
#[derive(Debug)]
pub struct Client {
    _stream: std::net::TcpStream,
}
impl Client {
    pub fn connect(_addr: impl ToSocketAddrs) -> Result<Client, ClientError> {
        todo!()
    }
    pub fn set_timeout(&self, _t: Option<Duration>) -> io::Result<()> {
        todo!()
    }
    pub fn query(&mut self, _sql: &str) -> Result<Output, ClientError> {
        todo!()
    }
}
