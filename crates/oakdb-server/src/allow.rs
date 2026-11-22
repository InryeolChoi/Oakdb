//! The allowlist of client addresses (design 0020, section 8-1).
//!
//! A tiny `pg_hba.conf`: IPv4 addresses or CIDR blocks, one per line, `#`
//! comments. Loopback clients are always let in; with no other entry the
//! server listens on 127.0.0.1 only. **This is not authentication**: an
//! address says where a client is, not who it is, and the traffic is not
//! encrypted.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

/// An IPv4 block: an address and how many leading bits of it count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ipv4Net {
    addr: Ipv4Addr,
    prefix: u8,
}

/// 127.0.0.0/8.
const LOOPBACK: Ipv4Net = Ipv4Net {
    addr: Ipv4Addr::new(127, 0, 0, 0),
    prefix: 8,
};

impl Ipv4Net {
    fn mask(prefix: u8) -> u32 {
        match prefix {
            0 => 0,
            p => u32::MAX << (32 - u32::from(p)),
        }
    }

    /// Whether `ip` is in the block.
    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        let mask = Self::mask(self.prefix);
        u32::from(ip) & mask == u32::from(self.addr) & mask
    }

    /// Whether every address of the block is a loopback address.
    fn is_loopback(&self) -> bool {
        self.prefix >= LOOPBACK.prefix && LOOPBACK.contains(self.addr)
    }
}

impl fmt::Display for Ipv4Net {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

impl FromStr for Ipv4Net {
    type Err = String;

    /// `10.0.0.5` (the same as `10.0.0.5/32`) or `10.0.0.0/24`.
    fn from_str(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let (addr, prefix) = match s.split_once('/') {
            Some((addr, prefix)) => (addr, Some(prefix)),
            None => (s, None),
        };
        let addr: Ipv4Addr = addr.parse().map_err(|_| {
            if addr.parse::<Ipv6Addr>().is_ok() {
                format!("{s}: IPv6 is not supported yet")
            } else {
                format!("{s}: not an IPv4 address or CIDR block")
            }
        })?;
        let prefix = match prefix {
            None => 32,
            Some(p) => p
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= 32)
                .ok_or_else(|| format!("{s}: the prefix length must be 0 to 32"))?,
        };
        let host_bits = u32::from(addr) & !Self::mask(prefix);
        if host_bits != 0 {
            let network = Ipv4Addr::from(u32::from(addr) & Self::mask(prefix));
            return Err(format!(
                "{s}: has bits set past the prefix; write {network}/{prefix} for the block, \
                 or {addr} for the one address"
            ));
        }
        Ok(Ipv4Net { addr, prefix })
    }
}

/// A line of an allowlist file that is not an entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowError {
    /// The line number, from 1.
    pub line: usize,
    /// The line, without its comment and surrounding blanks.
    pub text: String,
    /// What is wrong with it.
    pub reason: String,
}

impl fmt::Display for AllowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.line, self.reason)
    }
}

/// Which client addresses the server lets in.
#[derive(Debug, Clone)]
pub struct Allowlist {
    nets: Vec<Ipv4Net>,
    /// Loopback is let in whatever the list says. Only [`Allowlist::strict`]
    /// turns it off.
    loopback: bool,
}

impl Default for Allowlist {
    fn default() -> Self {
        Allowlist::loopback_only()
    }
}

impl Allowlist {
    /// No list: loopback clients only.
    pub fn loopback_only() -> Self {
        Allowlist::new(Vec::new())
    }

    /// `nets`, and loopback.
    pub fn new(nets: Vec<Ipv4Net>) -> Self {
        Allowlist {
            nets,
            loopback: true,
        }
    }

    /// `nets` and nothing else, not even loopback. The command line never
    /// builds this; tests use it to turn a loopback client away.
    pub fn strict(nets: Vec<Ipv4Net>) -> Self {
        Allowlist {
            nets,
            loopback: false,
        }
    }

    /// The entries of an allowlist file: one per line, blank lines
    /// skipped, `#` to the end of a line a comment.
    pub fn parse_file(text: &str) -> Result<Vec<Ipv4Net>, AllowError> {
        let mut nets = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let entry = line.split('#').next().unwrap_or("").trim();
            if entry.is_empty() {
                continue;
            }
            nets.push(entry.parse().map_err(|reason| AllowError {
                line: i + 1,
                text: entry.to_string(),
                reason,
            })?);
        }
        Ok(nets)
    }

    /// Whether a client at `ip` may connect. An IPv4 address written as
    /// IPv6 (`::ffff:10.0.0.5`) is matched as IPv4.
    pub fn allows(&self, ip: IpAddr) -> bool {
        let v4 = match ip {
            IpAddr::V4(v4) => v4,
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => v4,
                None => return false,
            },
        };
        (self.loopback && LOOPBACK.contains(v4)) || self.nets.iter().any(|n| n.contains(v4))
    }

    /// Whether the list lets in every address (`0.0.0.0/0`).
    pub fn allows_everyone(&self) -> bool {
        self.nets.iter().any(|n| n.prefix == 0)
    }

    /// Where the server listens: 127.0.0.1 unless some entry reaches past
    /// loopback, and then every IPv4 address.
    pub fn bind_ip(&self) -> Ipv4Addr {
        if self.nets.iter().all(Ipv4Net::is_loopback) {
            Ipv4Addr::LOCALHOST
        } else {
            Ipv4Addr::UNSPECIFIED
        }
    }
}
