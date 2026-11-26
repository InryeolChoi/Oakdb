//! The CLI client (design 0020, section 7): reads lines, cuts them into
//! statements at each `;` outside quotes, sends each one, and prints the
//! result as a table.

use std::io::{self, BufRead, Write};

use oakdb_sql::{transaction_command, Output, TransactionCommand, Value};

use crate::client::{Client, ClientError};
use crate::protocol::{closes, code};

/// Gathers lines until a `;` outside single quotes ends a statement.
#[derive(Debug, Default)]
pub struct Splitter {
    buf: String,
    in_quote: bool,
}

impl Splitter {
    /// Nothing gathered yet.
    pub fn new() -> Self {
        Splitter::default()
    }

    /// Adds one line (without its line break) and returns the statements
    /// it ends, without their `;`. Empty statements are dropped.
    pub fn push_line(&mut self, line: &str) -> Vec<String> {
        if !self.buf.is_empty() {
            self.buf.push('\n');
        }
        let mut done = Vec::new();
        for c in line.chars() {
            match c {
                // `''` inside a string toggles twice, so it stays inside.
                '\'' => {
                    self.in_quote = !self.in_quote;
                    self.buf.push(c);
                }
                ';' if !self.in_quote => {
                    let statement = self.buf.trim();
                    if !statement.is_empty() {
                        done.push(statement.to_string());
                    }
                    self.buf.clear();
                }
                _ => self.buf.push(c),
            }
        }
        done
    }

    /// Whether nothing but blanks is gathered.
    pub fn is_empty(&self) -> bool {
        self.buf.trim().is_empty()
    }

    /// What is left at the end of input, as a last statement.
    pub fn finish(&mut self) -> Option<String> {
        let statement = self.buf.trim().to_string();
        self.buf.clear();
        self.in_quote = false;
        (!statement.is_empty()).then_some(statement)
    }
}

/// How a result prints: rows as a table with a count, a changed row count
/// as the statement's first word and the number, anything else as `OK`.
pub fn format_output(sql: &str, output: &Output) -> String {
    match output {
        Output::Rows { columns, rows } => table(columns, rows),
        Output::Affected(n) => {
            let verb = sql.split_whitespace().next().unwrap_or("").to_uppercase();
            format!("{verb} {n}\n")
        }
        Output::Done => "OK\n".to_string(),
    }
}

fn table(columns: &[String], rows: &[Vec<Value>]) -> String {
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|row| row.iter().map(show).collect())
        .collect();
    let widths: Vec<usize> = columns
        .iter()
        .enumerate()
        .map(|(i, name)| {
            cells
                .iter()
                .filter_map(|row| row.get(i))
                .map(|cell| cell.chars().count())
                .chain([name.chars().count()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    let header: Vec<String> = columns
        .iter()
        .zip(&widths)
        .map(|(name, w)| format!("{name:<w$}"))
        .collect();
    line(&mut out, &format!(" {}", header.join(" | ")));
    let rule: Vec<String> = widths.iter().map(|w| "-".repeat(w + 2)).collect();
    line(&mut out, &rule.join("+"));
    for (row, values) in cells.iter().zip(rows) {
        let shown: Vec<String> = row
            .iter()
            .zip(values)
            .zip(&widths)
            .map(|((cell, value), w)| match value {
                Value::Integer(_) => format!("{cell:>w$}"),
                _ => format!("{cell:<w$}"),
            })
            .collect();
        line(&mut out, &format!(" {}", shown.join(" | ")));
    }
    let count = rows.len();
    out.push_str(&format!(
        "({count} {})\n",
        if count == 1 { "row" } else { "rows" }
    ));
    out
}

fn line(out: &mut String, text: &str) {
    out.push_str(text.trim_end());
    out.push('\n');
}

/// A value as the CLI shows it: BLOBs as SQL literals, so they can be
/// pasted back.
fn show(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Text(s) => s.clone(),
        Value::Blob(b) => {
            let hex: String = b.iter().map(|byte| format!("{byte:02x}")).collect();
            format!("X'{hex}'")
        }
    }
}

/// The prompt before a new statement: `oakdb*> ` while a transaction is
/// open, as psql marks it (design 0025 Q9), `oakdb> ` otherwise.
pub fn prompt(in_transaction: bool) -> &'static str {
    if in_transaction {
        "oakdb*> "
    } else {
        "oakdb> "
    }
}

/// Runs statements from `input` until it ends or `\q`, results to `out`
/// and errors to `err`. Prompts only when `interactive`. Returns the exit
/// code: 0, or 1 if any statement failed or the connection was lost.
///
/// Ending, by `\q` or the end of input, closes the connection, and the
/// server rolls back a transaction left open; the CLI says so on `err`.
/// When the server closes the connection on a transaction, the CLI says it
/// was rolled back and exits 1, without connecting again (design 0025 Q9).
pub fn run(
    client: &mut Client,
    input: impl BufRead,
    out: &mut impl Write,
    err: &mut impl Write,
    interactive: bool,
) -> io::Result<u8> {
    let mut splitter = Splitter::new();
    let mut failed = false;
    let mut lines = input.lines();
    loop {
        if interactive {
            let prompt = if splitter.is_empty() {
                prompt(client.in_transaction())
            } else {
                "   ...> "
            };
            write!(out, "{prompt}")?;
            out.flush()?;
        }
        let Some(line) = lines.next() else {
            if interactive {
                writeln!(out)?;
            }
            break;
        };
        let line = line?;
        if splitter.is_empty() && line.trim() == "\\q" {
            left_open(client, err)?;
            return Ok(u8::from(failed));
        }
        for statement in splitter.push_line(&line) {
            match send(client, &statement, out, err)? {
                Sent::Ok => {}
                Sent::Failed => failed = true,
                Sent::Lost => return Ok(1),
            }
        }
    }
    // A statement left without its `;` runs when input is a script, and is
    // dropped when a person pressed Ctrl-D halfway through it, as psql
    // does (review P8).
    let last = splitter.finish().filter(|_| !interactive);
    if let Some(statement) = last {
        match send(client, &statement, out, err)? {
            Sent::Ok => {}
            Sent::Failed => failed = true,
            Sent::Lost => return Ok(1),
        }
    }
    left_open(client, err)?;
    Ok(u8::from(failed))
}

/// Says that the transaction still open as the CLI leaves is rolled back.
fn left_open(client: &Client, err: &mut impl Write) -> io::Result<()> {
    if client.in_transaction() {
        writeln!(
            err,
            "the open transaction was not committed; it is rolled back as the connection closes"
        )?;
    }
    Ok(())
}

enum Sent {
    Ok,
    Failed,
    Lost,
}

fn send(
    client: &mut Client,
    statement: &str,
    out: &mut impl Write,
    err: &mut impl Write,
) -> io::Result<Sent> {
    let was_open = client.in_transaction();
    let result = client.query(statement);
    let lost = match &result {
        Err(ClientError::Server { code, .. }) => closes(*code),
        Err(_) => true,
        Ok(_) => false,
    };
    if lost && was_open {
        // The error says why; the line after it says what became of the
        // transaction.
        if let Err(e) = &result {
            writeln!(err, "{e}")?;
        }
        // A server that closes on idling or shutting down rolled back
        // before it said so, and a COMMIT it answers so was not run;
        // otherwise a COMMIT may have gone through before the end.
        let rolled_back = matches!(
            result,
            Err(ClientError::Server {
                code: code::IDLE_IN_TRANSACTION | code::SHUTTING_DOWN,
                ..
            })
        );
        if !rolled_back && transaction_command(statement) == Some(TransactionCommand::Commit) {
            writeln!(
                err,
                "the connection closed before COMMIT was answered; the transaction may or may \
                 not have been committed"
            )?;
        } else {
            writeln!(
                err,
                "connection closed by server; the open transaction was rolled back"
            )?;
        }
        return Ok(Sent::Lost);
    }
    match result {
        Ok(output) => {
            write!(out, "{}", format_output(statement, &output))?;
            out.flush()?;
            Ok(Sent::Ok)
        }
        // An error after which the server closes ends the session now,
        // not at the next statement (review P7).
        Err(e @ ClientError::Server { code, .. }) => {
            writeln!(err, "{e}")?;
            Ok(if closes(code) {
                Sent::Lost
            } else {
                Sent::Failed
            })
        }
        Err(e) => {
            writeln!(err, "{e}")?;
            Ok(Sent::Lost)
        }
    }
}
