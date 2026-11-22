//! Stub.
use std::net::{IpAddr, Ipv4Addr};
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ipv4Net {
    addr: Ipv4Addr,
    prefix: u8,
}
impl Ipv4Net {
    pub fn contains(&self, _ip: Ipv4Addr) -> bool {
        let _ = (self.addr, self.prefix);
        todo!()
    }
}
impl FromStr for Ipv4Net {
    type Err = String;
    fn from_str(_s: &str) -> Result<Self, String> {
        todo!()
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowError {
    pub line: usize,
    pub text: String,
    pub reason: String,
}
#[derive(Debug, Clone, Default)]
pub struct Allowlist {
    nets: Vec<Ipv4Net>,
    loopback: bool,
}
impl Allowlist {
    pub fn loopback_only() -> Self {
        todo!()
    }
    pub fn new(_nets: Vec<Ipv4Net>) -> Self {
        todo!()
    }
    pub fn strict(_nets: Vec<Ipv4Net>) -> Self {
        todo!()
    }
    pub fn parse_file(_text: &str) -> Result<Vec<Ipv4Net>, AllowError> {
        todo!()
    }
    pub fn allows(&self, _ip: IpAddr) -> bool {
        let _ = (&self.nets, self.loopback);
        todo!()
    }
    pub fn allows_everyone(&self) -> bool {
        todo!()
    }
    pub fn bind_ip(&self) -> Ipv4Addr {
        todo!()
    }
}
