//! A client: connects, says HELLO, and sends one statement at a time.

use std::fmt;
use std::io::{self, BufReader};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use oakdb_sql::{transaction_command, Output, TransactionCommand};

use crate::protocol::{closes, read_frame, write_frame, FrameError, Request, Response, VERSION};

/// Why a request got no result.
#[derive(Debug)]
pub enum ClientError {
    /// The network failed.
    Io(io::Error),
    /// The server sent something this client cannot read.
    Protocol(String),
    /// The server answered with an error.
    Server {
        /// One of [`crate::protocol::code`].
        code: u8,
        /// What failed.
        message: String,
    },
    /// The server closed the connection.
    Closed,
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Io(e) => write!(f, "{e}"),
            ClientError::Protocol(what) => write!(f, "protocol error: {what}"),
            ClientError::Server { code, message } => write!(f, "ERROR 0x{code:02x} {message}"),
            ClientError::Closed => f.write_str("connection closed by server"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<io::Error> for ClientError {
    fn from(e: io::Error) -> Self {
        ClientError::Io(e)
    }
}

/// A connection to an Oakdb server.
#[derive(Debug)]
pub struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    /// Whether the server's session for this connection has a transaction
    /// open, worked out from what was sent and answered.
    in_transaction: bool,
}

impl Client {
    /// Connects and does the handshake. A server that turns the client
    /// away (allowlist, connection limit, version) gives
    /// [`ClientError::Server`].
    pub fn connect(addr: impl ToSocketAddrs) -> Result<Client, ClientError> {
        let writer = TcpStream::connect(addr)?;
        writer.set_nodelay(true)?;
        let mut client = Client {
            reader: BufReader::new(writer.try_clone()?),
            writer,
            in_transaction: false,
        };
        match client.call(&Request::Hello { version: VERSION })? {
            Response::Ready { version: VERSION } => Ok(client),
            Response::Ready { version } => Err(ClientError::Protocol(format!(
                "the server speaks protocol {version}, this client speaks {VERSION}"
            ))),
            other => Err(unexpected(&other)),
        }
    }

    /// How long to wait on the server before failing; `None` waits forever.
    pub fn set_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.writer.set_read_timeout(timeout)?;
        self.writer.set_write_timeout(timeout)
    }

    /// Runs one SQL statement on the server. BEGIN, COMMIT and ROLLBACK
    /// open and end a transaction that lasts across calls; see
    /// [`Client::in_transaction`].
    pub fn query(&mut self, sql: &str) -> Result<Output, ClientError> {
        let result = self.call(&Request::Query(sql.to_string()));
        self.in_transaction = match (&result, transaction_command(sql)) {
            // The connection is over, and with it the transaction.
            (Err(ClientError::Server { code, .. }), _) if closes(*code) => false,
            (Err(ClientError::Io(_) | ClientError::Closed | ClientError::Protocol(_)), _) => false,
            (Ok(Response::Done), Some(TransactionCommand::Begin)) => true,
            // COMMIT and ROLLBACK end it whatever the answer: a conflict
            // or a failed sync ends it too, and one sent with none open
            // found none.
            (_, Some(TransactionCommand::Commit | TransactionCommand::Rollback)) => false,
            // A BEGIN refused, or any other statement: no change.
            _ => self.in_transaction,
        };
        match result? {
            Response::Rows { columns, rows } => Ok(Output::Rows { columns, rows }),
            Response::Affected(n) => Ok(Output::Affected(n)),
            Response::Done => Ok(Output::Done),
            other => Err(unexpected(&other)),
        }
    }

    /// Whether this connection has a transaction open on the server: a
    /// BEGIN was answered `DONE`, and no COMMIT or ROLLBACK was sent since
    /// (issue #37, design 0025 Q3). The client works it out from what it
    /// sent and what came back, by the protocol's rule; it asks the server
    /// nothing. Once the connection breaks, it is `false`: the server
    /// rolls back the transaction of a connection that ends.
    pub fn in_transaction(&self) -> bool {
        self.in_transaction
    }

    /// Sends `request` and reads its response; an `ERROR` response becomes
    /// [`ClientError::Server`].
    fn call(&mut self, request: &Request) -> Result<Response, ClientError> {
        let frame = request.encode();
        if let Err(e) = write_frame(&mut self.writer, &frame) {
            // The server may have refused us and closed: its error is
            // worth more than the broken pipe.
            return match self.read() {
                Ok(Response::Error { code, message }) => Err(ClientError::Server { code, message }),
                _ => Err(ClientError::Io(e)),
            };
        }
        match self.read()? {
            Response::Error { code, message } => Err(ClientError::Server { code, message }),
            response => Ok(response),
        }
    }

    fn read(&mut self) -> Result<Response, ClientError> {
        let frame = match read_frame(&mut self.reader) {
            Ok(Some(frame)) => frame,
            Ok(None) | Err(FrameError::Truncated) => return Err(ClientError::Closed),
            Err(FrameError::Io(e)) => return Err(ClientError::Io(e)),
            Err(e) => return Err(ClientError::Protocol(e.to_string())),
        };
        Response::decode(frame).map_err(|e| ClientError::Protocol(e.to_string()))
    }
}

fn unexpected(response: &Response) -> ClientError {
    ClientError::Protocol(format!("unexpected response {response:?}"))
}
