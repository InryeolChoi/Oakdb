//! Stub.
use oakdb_sql::{Output, SqlError, Value};
use std::io::{self, Read, Write};

pub const MAGIC: [u8; 5] = *b"OAKDB";
pub const VERSION: u16 = 1;
pub const MAX_FRAME_LEN: u32 = 16 * 1024 * 1024;

pub mod command {
    pub const HELLO: u8 = 0x01;
    pub const QUERY: u8 = 0x02;
    pub const READY: u8 = 0x81;
    pub const ROWS: u8 = 0x82;
    pub const AFFECTED: u8 = 0x83;
    pub const DONE: u8 = 0x84;
    pub const ERROR: u8 = 0x85;
}

pub mod code {
    pub const SYNTAX: u8 = 0x01;
    pub const UNSUPPORTED: u8 = 0x02;
    pub const NO_SUCH_TABLE: u8 = 0x03;
    pub const NO_SUCH_COLUMN: u8 = 0x04;
    pub const TABLE_EXISTS: u8 = 0x05;
    pub const DUPLICATE_COLUMN: u8 = 0x06;
    pub const VALUE_COUNT: u8 = 0x07;
    pub const DUPLICATE_KEY: u8 = 0x08;
    pub const TYPE_MISMATCH: u8 = 0x09;
    pub const SQL_CORRUPT: u8 = 0x0a;
    pub const OTHER_STATEMENT: u8 = 0x1f;
    pub const CONFLICT: u8 = 0x20;
    pub const KEY_TOO_LARGE: u8 = 0x21;
    pub const VALUE_TOO_LARGE: u8 = 0x22;
    pub const CORRUPT: u8 = 0x23;
    pub const IO: u8 = 0x24;
    pub const OUTCOME_UNKNOWN: u8 = 0x25;
    pub const STOPPED: u8 = 0x26;
    pub const OTHER_ENGINE: u8 = 0x3f;
    pub const MALFORMED: u8 = 0x40;
    pub const FRAME_TOO_LARGE: u8 = 0x41;
    pub const HANDSHAKE: u8 = 0x42;
    pub const NOT_UTF8: u8 = 0x43;
    pub const RESULT_TOO_LARGE: u8 = 0x44;
    pub const TOO_MANY_CONNECTIONS: u8 = 0x45;
    pub const NOT_ALLOWED: u8 = 0x46;
    pub const INTERNAL: u8 = 0x7f;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub command: u8,
    pub payload: Vec<u8>,
}

#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    Truncated,
    Empty,
    TooLarge(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    UnknownCommand(u8),
    Malformed(&'static str),
    BadMagic,
    NotUtf8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Hello { version: u16 },
    Query(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Ready {
        version: u16,
    },
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
    },
    Affected(u64),
    Done,
    Error {
        code: u8,
        message: String,
    },
}

pub fn read_frame(_r: &mut impl Read) -> Result<Option<Frame>, FrameError> {
    todo!()
}
pub fn write_frame(_w: &mut impl Write, _frame: &Frame) -> io::Result<()> {
    todo!()
}
pub fn error_code(_e: &SqlError) -> u8 {
    todo!()
}
impl Request {
    pub fn encode(&self) -> Frame {
        todo!()
    }
    pub fn decode(_frame: &Frame) -> Result<Request, DecodeError> {
        todo!()
    }
}
impl Response {
    pub fn encode(&self) -> Frame {
        todo!()
    }
    pub fn decode(_frame: &Frame) -> Result<Response, DecodeError> {
        todo!()
    }
}
impl From<Output> for Response {
    fn from(_o: Output) -> Self {
        todo!()
    }
}
