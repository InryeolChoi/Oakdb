//! The wire protocol (design 0020, sections 2 to 4; docs/protocol.md).
//!
//! Every message is a frame: a 4-byte big-endian length, then a command
//! byte, then the command's data. The length counts the command byte and
//! the data, not itself, and is at most [`MAX_FRAME_LEN`]. A client opens
//! with `HELLO`, the server answers `READY`, and from then on each `QUERY`
//! gets exactly one response, in order.
//!
//! Nothing here trusts the bytes it reads: a frame that claims more than
//! the limit is refused before its body is read, and every decoder checks
//! each length against what is left, so no input makes it panic.

use std::fmt;
use std::io::{self, Read, Write};

use oakdb_sql::{Output, SqlError, Value};

/// What a `HELLO` starts with, so the server can tell an Oakdb client from
/// anything else that knocks.
pub const MAGIC: [u8; 5] = *b"OAKDB";

/// The protocol version this build speaks.
pub const VERSION: u16 = 1;

/// Largest length a frame may give: its command byte and data together.
pub const MAX_FRAME_LEN: u32 = 16 * 1024 * 1024;

/// Command bytes. Responses have the top bit set.
pub mod command {
    /// Client opens: magic and version.
    pub const HELLO: u8 = 0x01;
    /// Client sends one SQL statement, as UTF-8.
    pub const QUERY: u8 = 0x02;
    /// Server accepts the HELLO: its version.
    pub const READY: u8 = 0x81;
    /// Rows a SELECT found.
    pub const ROWS: u8 = 0x82;
    /// How many rows a statement changed.
    pub const AFFECTED: u8 = 0x83;
    /// The statement ran and has nothing to show.
    pub const DONE: u8 = 0x84;
    /// Something failed: an error code and a message.
    pub const ERROR: u8 = 0x85;
}

/// Error codes of an `ERROR` frame (design 0020, section 4). `0x01–0x1F`:
/// the statement is wrong; `0x20–0x3F`: the engine refused or stopped;
/// `0x40–0x5F`: the conversation broke. The server closes the connection
/// after `MALFORMED`, `FRAME_TOO_LARGE`, `HANDSHAKE`, `TOO_MANY_CONNECTIONS`,
/// `NOT_ALLOWED` and `INTERNAL`, and keeps it after the rest.
pub mod code {
    /// The statement could not be read.
    pub const SYNTAX: u8 = 0x01;
    /// SQL that Oakdb does not support.
    pub const UNSUPPORTED: u8 = 0x02;
    /// No such table.
    pub const NO_SUCH_TABLE: u8 = 0x03;
    /// No such column.
    pub const NO_SUCH_COLUMN: u8 = 0x04;
    /// The table already exists.
    pub const TABLE_EXISTS: u8 = 0x05;
    /// A column is named twice.
    pub const DUPLICATE_COLUMN: u8 = 0x06;
    /// A row holds the wrong number of values.
    pub const VALUE_COUNT: u8 = 0x07;
    /// A row with this primary key exists.
    pub const DUPLICATE_KEY: u8 = 0x08;
    /// A value does not fit its column.
    pub const TYPE_MISMATCH: u8 = 0x09;
    /// What the SQL layer stored cannot be read back.
    pub const SQL_CORRUPT: u8 = 0x0a;
    /// A statement error of a kind this build has no code for.
    pub const OTHER_STATEMENT: u8 = 0x1f;
    /// Another transaction committed a write to the same key first. Running
    /// the statement again may work.
    pub const CONFLICT: u8 = 0x20;
    /// A key is too long.
    pub const KEY_TOO_LARGE: u8 = 0x21;
    /// A value is too long.
    pub const VALUE_TOO_LARGE: u8 = 0x22;
    /// The data file is damaged.
    pub const CORRUPT: u8 = 0x23;
    /// The file system failed.
    pub const IO: u8 = 0x24;
    /// The disk failed during this commit: it may or may not be saved. The
    /// engine stopped.
    pub const OUTCOME_UNKNOWN: u8 = 0x25;
    /// The engine stopped after a disk failure; restart the server.
    pub const STOPPED: u8 = 0x26;
    /// An engine error of a kind this build has no code for.
    pub const OTHER_ENGINE: u8 = 0x3f;
    /// A frame of length zero, an unknown command, or data of the wrong
    /// shape.
    pub const MALFORMED: u8 = 0x40;
    /// A frame longer than [`super::MAX_FRAME_LEN`].
    pub const FRAME_TOO_LARGE: u8 = 0x41;
    /// The first frame is not a HELLO of this magic and version.
    pub const HANDSHAKE: u8 = 0x42;
    /// The SQL is not UTF-8.
    pub const NOT_UTF8: u8 = 0x43;
    /// The result does not fit in one frame.
    pub const RESULT_TOO_LARGE: u8 = 0x44;
    /// The server has as many connections as it takes.
    pub const TOO_MANY_CONNECTIONS: u8 = 0x45;
    /// The client's address is not on the server's allowlist.
    pub const NOT_ALLOWED: u8 = 0x46;
    /// The server failed in a way it should not.
    pub const INTERNAL: u8 = 0x7f;
}

/// One message: a command byte and its data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// One of [`command`].
    pub command: u8,
    /// The data after the command byte.
    pub payload: Vec<u8>,
}

/// Why a frame could not be read off a stream.
#[derive(Debug)]
pub enum FrameError {
    /// Reading failed.
    Io(io::Error),
    /// The stream ended inside a frame.
    Truncated,
    /// The length is zero, so there is not even a command byte.
    Empty,
    /// The length is over [`MAX_FRAME_LEN`]. The body was not read.
    TooLarge(u32),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Io(e) => write!(f, "reading a frame failed: {e}"),
            FrameError::Truncated => f.write_str("the stream ended inside a frame"),
            FrameError::Empty => f.write_str("a frame of length 0 has no command"),
            FrameError::TooLarge(len) => write!(
                f,
                "a frame of length {len} is over the {MAX_FRAME_LEN} byte limit"
            ),
        }
    }
}

/// Why a frame's data is not the message its command says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// No message has this command byte.
    UnknownCommand(u8),
    /// The data does not have the shape of the command. Says what is
    /// wrong.
    Malformed(&'static str),
    /// A HELLO that does not start with [`MAGIC`].
    BadMagic,
    /// A QUERY whose SQL is not UTF-8.
    NotUtf8,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::UnknownCommand(c) => write!(f, "unknown command 0x{c:02x}"),
            DecodeError::Malformed(what) => write!(f, "malformed frame: {what}"),
            DecodeError::BadMagic => f.write_str("HELLO does not start with OAKDB"),
            DecodeError::NotUtf8 => f.write_str("the SQL is not UTF-8"),
        }
    }
}

/// What a client sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// The first frame of every connection.
    Hello {
        /// The protocol version the client speaks.
        version: u16,
    },
    /// One SQL statement.
    Query(String),
}

/// What the server sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// The HELLO is accepted.
    Ready {
        /// The protocol version the server speaks.
        version: u16,
    },
    /// The rows a SELECT found.
    Rows {
        /// Names of the columns.
        columns: Vec<String>,
        /// The rows, each with one value per column.
        rows: Vec<Vec<Value>>,
    },
    /// How many rows the statement changed.
    Affected(u64),
    /// The statement ran and has nothing to show.
    Done,
    /// Something failed.
    Error {
        /// One of [`code`].
        code: u8,
        /// What failed, for people.
        message: String,
    },
}

/// Reads one frame. `Ok(None)` means the stream ended cleanly, between
/// frames.
pub fn read_frame(r: &mut impl Read) -> Result<Option<Frame>, FrameError> {
    let mut header = [0u8; 4];
    let mut got = 0;
    while got < header.len() {
        match r.read(&mut header[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(FrameError::Truncated),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(FrameError::Io(e)),
        }
    }
    let len = u32::from_be_bytes(header);
    if len == 0 {
        return Err(FrameError::Empty);
    }
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof => FrameError::Truncated,
        _ => FrameError::Io(e),
    })?;
    let payload = body.split_off(1);
    Ok(Some(Frame {
        command: body[0],
        payload,
    }))
}

/// Writes one frame in a single write, so it leaves in as few packets as
/// it can. Fails without writing if the frame is over the limit.
pub fn write_frame(w: &mut impl Write, frame: &Frame) -> io::Result<()> {
    let len = frame_len(frame).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("a frame of {} bytes is over the limit", frame.payload.len()),
        )
    })?;
    let mut bytes = Vec::with_capacity(5 + frame.payload.len());
    bytes.extend_from_slice(&len.to_be_bytes());
    bytes.push(frame.command);
    bytes.extend_from_slice(&frame.payload);
    w.write_all(&bytes)?;
    w.flush()
}

/// The length field of `frame`, or `None` if it is over the limit.
pub fn frame_len(frame: &Frame) -> Option<u32> {
    u32::try_from(frame.payload.len() + 1)
        .ok()
        .filter(|len| *len <= MAX_FRAME_LEN)
}

/// The code an `ERROR` frame carries for `e`.
pub fn error_code(e: &SqlError) -> u8 {
    match e {
        SqlError::Syntax { .. } => code::SYNTAX,
        SqlError::Unsupported(_) => code::UNSUPPORTED,
        SqlError::NoSuchTable(_) => code::NO_SUCH_TABLE,
        SqlError::NoSuchColumn { .. } => code::NO_SUCH_COLUMN,
        SqlError::TableExists(_) => code::TABLE_EXISTS,
        SqlError::DuplicateColumn(_) => code::DUPLICATE_COLUMN,
        SqlError::ValueCount { .. } => code::VALUE_COUNT,
        SqlError::DuplicateKey { .. } => code::DUPLICATE_KEY,
        SqlError::TypeMismatch { .. } => code::TYPE_MISMATCH,
        SqlError::Corrupt(_) => code::SQL_CORRUPT,
        SqlError::Engine(e) => match e {
            oakdb::Error::Conflict { .. } => code::CONFLICT,
            oakdb::Error::KeyTooLarge { .. } => code::KEY_TOO_LARGE,
            oakdb::Error::ValueTooLarge { .. } => code::VALUE_TOO_LARGE,
            oakdb::Error::Corrupt(_) => code::CORRUPT,
            oakdb::Error::Io(_) => code::IO,
            oakdb::Error::OutcomeUnknown(_) => code::OUTCOME_UNKNOWN,
            oakdb::Error::Stopped(_) => code::STOPPED,
            _ => code::OTHER_ENGINE,
        },
        _ => code::OTHER_STATEMENT,
    }
}

impl Request {
    /// The frame that carries this request.
    pub fn encode(&self) -> Frame {
        match self {
            Request::Hello { version } => {
                let mut payload = MAGIC.to_vec();
                payload.extend_from_slice(&version.to_be_bytes());
                Frame {
                    command: command::HELLO,
                    payload,
                }
            }
            Request::Query(sql) => Frame {
                command: command::QUERY,
                payload: sql.as_bytes().to_vec(),
            },
        }
    }

    /// The request `frame` carries.
    pub fn decode(frame: &Frame) -> Result<Request, DecodeError> {
        match frame.command {
            command::HELLO => {
                let mut r = Reader::new(&frame.payload);
                if r.take(MAGIC.len()).ok() != Some(&MAGIC[..]) {
                    return Err(DecodeError::BadMagic);
                }
                let version = r.u16()?;
                r.end()?;
                Ok(Request::Hello { version })
            }
            command::QUERY => String::from_utf8(frame.payload.clone())
                .map(Request::Query)
                .map_err(|_| DecodeError::NotUtf8),
            other => Err(DecodeError::UnknownCommand(other)),
        }
    }
}

impl Response {
    /// The frame that carries this response. It may be over the limit;
    /// check with [`frame_len`] before sending.
    pub fn encode(&self) -> Frame {
        let mut p = Vec::new();
        let command = match self {
            Response::Ready { version } => {
                p.extend_from_slice(&version.to_be_bytes());
                command::READY
            }
            Response::Rows { columns, rows } => {
                put_u16_len(&mut p, columns.len());
                for name in columns {
                    put_u16_len(&mut p, name.len());
                    p.extend_from_slice(name.as_bytes());
                }
                p.extend_from_slice(&u32::try_from(rows.len()).unwrap_or(u32::MAX).to_be_bytes());
                for row in rows {
                    for value in row {
                        put_value(&mut p, value);
                    }
                }
                command::ROWS
            }
            Response::Affected(n) => {
                p.extend_from_slice(&n.to_be_bytes());
                command::AFFECTED
            }
            Response::Done => command::DONE,
            Response::Error { code, message } => {
                p.push(*code);
                p.extend_from_slice(message.as_bytes());
                command::ERROR
            }
        };
        Frame {
            command,
            payload: p,
        }
    }

    /// The response `frame` carries.
    pub fn decode(frame: &Frame) -> Result<Response, DecodeError> {
        let mut r = Reader::new(&frame.payload);
        let response = match frame.command {
            command::READY => Response::Ready { version: r.u16()? },
            command::ROWS => {
                let width = r.u16()? as usize;
                let mut columns = Vec::with_capacity(width);
                for _ in 0..width {
                    let len = r.u16()? as usize;
                    columns.push(r.utf8(len)?);
                }
                let count = r.u32()?;
                if width == 0 && count > 0 {
                    return Err(DecodeError::Malformed("rows without columns"));
                }
                // Every row takes at least a byte per column, so the count
                // cannot claim more rows than the bytes left could hold.
                let mut rows = Vec::new();
                for _ in 0..count {
                    let mut row = Vec::with_capacity(width);
                    for _ in 0..width {
                        row.push(r.value()?);
                    }
                    rows.push(row);
                }
                Response::Rows { columns, rows }
            }
            command::AFFECTED => Response::Affected(u64::from_be_bytes(r.array()?)),
            command::DONE => Response::Done,
            command::ERROR => {
                let code = r.u8()?;
                let rest = r.rest();
                let message = String::from_utf8(rest.to_vec()).map_err(|_| DecodeError::NotUtf8)?;
                Response::Error { code, message }
            }
            other => return Err(DecodeError::UnknownCommand(other)),
        };
        r.end()?;
        Ok(response)
    }
}

impl From<Output> for Response {
    fn from(output: Output) -> Self {
        match output {
            Output::Rows { columns, rows } => Response::Rows { columns, rows },
            Output::Affected(n) => Response::Affected(n),
            Output::Done => Response::Done,
        }
    }
}

/// Value tags: the same as the SQL layer's row values (design 0017, 2-2).
const NULL: u8 = 0x00;
const INTEGER: u8 = 0x01;
const TEXT: u8 = 0x02;
const BLOB: u8 = 0x03;

fn put_u16_len(p: &mut Vec<u8>, len: usize) {
    // Column names are identifiers and there are few columns, so both fit;
    // saturating keeps a broken caller from panicking the server.
    p.extend_from_slice(&u16::try_from(len).unwrap_or(u16::MAX).to_be_bytes());
}

fn put_value(p: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => p.push(NULL),
        Value::Integer(i) => {
            p.push(INTEGER);
            p.extend_from_slice(&i.to_be_bytes());
        }
        Value::Text(s) => {
            p.push(TEXT);
            put_bytes(p, s.as_bytes());
        }
        Value::Blob(b) => {
            p.push(BLOB);
            put_bytes(p, b);
        }
    }
}

fn put_bytes(p: &mut Vec<u8>, bytes: &[u8]) {
    // A value longer than u32::MAX cannot be stored (the engine's limit),
    // and the frame limit catches long ones before they are sent.
    p.extend_from_slice(&u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_be_bytes());
    p.extend_from_slice(bytes);
}

/// Reads a payload front to back, checking every length against what is
/// left.
struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Reader { rest: bytes }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if n > self.rest.len() {
            return Err(DecodeError::Malformed("data ends early"));
        }
        let (head, tail) = self.rest.split_at(n);
        self.rest = tail;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    fn utf8(&mut self, len: usize) -> Result<String, DecodeError> {
        String::from_utf8(self.take(len)?.to_vec()).map_err(|_| DecodeError::NotUtf8)
    }

    fn value(&mut self) -> Result<Value, DecodeError> {
        Ok(match self.u8()? {
            NULL => Value::Null,
            INTEGER => Value::Integer(i64::from_be_bytes(self.array()?)),
            TEXT => {
                let len = self.u32()? as usize;
                Value::Text(self.utf8(len)?)
            }
            BLOB => {
                let len = self.u32()? as usize;
                Value::Blob(self.take(len)?.to_vec())
            }
            _ => return Err(DecodeError::Malformed("unknown value tag")),
        })
    }

    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.rest)
    }

    fn end(&self) -> Result<(), DecodeError> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::Malformed("data goes on past the message"))
        }
    }
}
