//! The CLI client (design 0020, section 7): reads lines, cuts them into
//! statements at each `;` outside quotes, sends each one, and prints the
//! result as a table.

use std::io::{self, BufRead, Write};

use oakdb_sql::{Output, Value};

use crate::client::{Client, ClientError};

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

/// Runs statements from `input` until it ends or `\q`, results to `out`
/// and errors to `err`. Prompts only when `interactive`. Returns the exit
/// code: 0, or 1 if any statement failed or the connection was lost.
pub fn run(
    client: &mut Client,
    input: impl BufRead,
    out: &mut impl Write,
    err: &mut impl Write,
    interactive: bool,
) -> io::Result<i32> {
    let mut splitter = Splitter::new();
    let mut failed = false;
    let mut lines = input.lines();
    loop {
        if interactive {
            let prompt = if splitter.is_empty() {
                "oakdb> "
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
            return Ok(i32::from(failed));
        }
        for statement in splitter.push_line(&line) {
            match send(client, &statement, out, err)? {
                Sent::Ok => {}
                Sent::Failed => failed = true,
                Sent::Lost => return Ok(1),
            }
        }
    }
    if let Some(statement) = splitter.finish() {
        match send(client, &statement, out, err)? {
            Sent::Ok => {}
            Sent::Failed => failed = true,
            Sent::Lost => return Ok(1),
        }
    }
    Ok(i32::from(failed))
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
    match client.query(statement) {
        Ok(output) => {
            write!(out, "{}", format_output(statement, &output))?;
            out.flush()?;
            Ok(Sent::Ok)
        }
        Err(e @ ClientError::Server { .. }) => {
            writeln!(err, "{e}")?;
            Ok(Sent::Failed)
        }
        Err(e) => {
            writeln!(err, "{e}")?;
            Ok(Sent::Lost)
        }
    }
}
