//! `oakdb serve` runs the server; `oakdb cli` connects to one (design
//! 0020, section 6).

use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use oakdb::Db;
use oakdb_server::allow::{Allowlist, Ipv4Net};
use oakdb_server::client::Client;
use oakdb_server::server::{
    Config, Server, ShutdownHandle, DEFAULT_IDLE_IN_TRANSACTION_TIMEOUT,
    DEFAULT_LONG_TRANSACTION_WARNING, DEFAULT_MAX_CONNECTIONS, DEFAULT_PORT,
    DEFAULT_SHUTDOWN_GRACE,
};

const USAGE: &str = "\
usage:
  oakdb serve --data <dir> [--port <n>] [--allow <ip or cidr>]... [--allow-file <file>]
              [--max-connections <n>] [--idle-in-transaction-timeout <s>]
              [--idle-timeout <s>] [--warn-transaction-age <s>]
              [--shutdown-grace <s>]
  oakdb cli [--host <host>] [--port <n>]

serve
  --data <dir>             database directory, created if missing (required)
  --port <n>               TCP port, 0 for any free one (default 7878)
  --allow <ip or cidr>     let clients from this IPv4 address or block in; repeatable
  --allow-file <file>      the same, one entry per line, # starts a comment
  --max-connections <n>    connections taken at once (default 512)
  --idle-in-transaction-timeout <s>
                           roll back and close a connection that waits this
                           many seconds for its next statement inside a
                           transaction (default 60, 0 for never)
  --idle-timeout <s>       the same outside a transaction (default 0, never)
  --warn-transaction-age <s>
                           log a warning when a transaction stays open this
                           many seconds (default 300, 0 for never)
  --shutdown-grace <s>     on shutdown, give open transactions this many
                           seconds to finish before rolling them back
                           (default 10, 0 to roll them back at once)

  SIGINT (Ctrl-C) or SIGTERM shuts the server down: new connections are
  refused, a statement running is answered, connections are closed with
  ERROR 0x49 (in a transaction, after the grace, rolled back), and the
  database is checkpointed. A second signal stops it at once.
  Exit codes: 0 shut down cleanly; 1 failed to start, or the final
  checkpoint failed, or statements still ran 5s past the grace (the next
  start replays the WAL); 2 wrong command line; 130 or 143 stopped by a
  second SIGINT or SIGTERM.

  With no allowlist the server listens on 127.0.0.1 and only local clients
  connect. Loopback clients are always let in. An entry outside loopback
  makes the server listen on every IPv4 address and turn away clients not
  on the list. THIS IS NOT AUTHENTICATION: an address is not an identity,
  and nothing is encrypted. Open it only to networks you trust.

cli
  --host <host>            server host (default 127.0.0.1)
  --port <n>               server port (default 7878)

  Statements end with `;`. `\\q` or end of input quits.";

/// A wrong command line: print the usage and exit 2.
struct Usage(String);

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `oakdb serve --help` and `oakdb cli --help` too, wherever it stands.
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let result = match args.first().map(String::as_str) {
        Some("serve") => serve(&args[1..]),
        Some("cli") => cli(&args[1..]),
        Some("help") => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some(other) => Err(Usage(format!("unknown command {other:?}"))),
        None => Err(Usage("no command".into())),
    };
    match result {
        Ok(code) => code,
        Err(Usage(message)) => {
            eprintln!("oakdb: {message}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// Fails the run with a message and exit code 1.
fn fail(message: impl std::fmt::Display) -> Result<ExitCode, Usage> {
    eprintln!("oakdb: {message}");
    Ok(ExitCode::from(1))
}

/// Walks `--flag value` pairs.
fn flags(args: &[String]) -> Result<Vec<(&str, &str)>, Usage> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        if !flag.starts_with("--") {
            return Err(Usage(format!("unexpected argument {flag:?}")));
        }
        let value = it
            .next()
            .ok_or_else(|| Usage(format!("{flag} needs a value")))?;
        out.push((flag.as_str(), value.as_str()));
    }
    Ok(out)
}

fn number<T: std::str::FromStr>(flag: &str, value: &str) -> Result<T, Usage> {
    value
        .parse()
        .map_err(|_| Usage(format!("{flag} {value:?} is not a number")))
}

/// A number of seconds, fractions allowed (`0.5`).
fn seconds(flag: &str, value: &str) -> Result<Duration, Usage> {
    value
        .parse::<f64>()
        .ok()
        .and_then(|s| Duration::try_from_secs_f64(s).ok())
        .ok_or_else(|| Usage(format!("{flag} {value:?} is not a number of seconds")))
}

fn serve(args: &[String]) -> Result<ExitCode, Usage> {
    let mut data: Option<PathBuf> = None;
    let mut port = DEFAULT_PORT;
    let mut max_connections = DEFAULT_MAX_CONNECTIONS;
    let mut nets: Vec<Ipv4Net> = Vec::new();
    let mut idle_in_transaction = DEFAULT_IDLE_IN_TRANSACTION_TIMEOUT;
    let mut idle = Duration::ZERO;
    let mut warn_age = DEFAULT_LONG_TRANSACTION_WARNING;
    let mut grace = DEFAULT_SHUTDOWN_GRACE;
    for (flag, value) in flags(args)? {
        match flag {
            "--data" => data = Some(PathBuf::from(value)),
            "--port" => port = number(flag, value)?,
            "--max-connections" => max_connections = number(flag, value)?,
            "--idle-in-transaction-timeout" => idle_in_transaction = seconds(flag, value)?,
            "--idle-timeout" => idle = seconds(flag, value)?,
            "--warn-transaction-age" => warn_age = seconds(flag, value)?,
            "--shutdown-grace" => grace = seconds(flag, value)?,
            "--allow" => match value.parse() {
                Ok(net) => nets.push(net),
                Err(reason) => return fail(format!("--allow {reason}")),
            },
            "--allow-file" => {
                let text = match std::fs::read_to_string(value) {
                    Ok(text) => text,
                    Err(e) => return fail(format!("cannot read {value}: {e}")),
                };
                match Allowlist::parse_file(&text) {
                    Ok(more) => nets.extend(more),
                    Err(e) => return fail(format!("{value}:{e}")),
                }
            }
            _ => return Err(Usage(format!("unknown flag {flag} for serve"))),
        }
    }
    let data = data.ok_or_else(|| Usage("serve needs --data <dir>".into()))?;
    if max_connections == 0 {
        return Err(Usage("--max-connections must be at least 1".into()));
    }
    let allow = Allowlist::new(nets);
    if allow.allows_everyone() {
        eprintln!("oakdb: warning: the allowlist lets in every address");
    }

    // Before the database opens: its commit thread must inherit the mask
    // (design 0028 Q1). No thread is made before this line.
    let handle = ShutdownHandle::new();
    #[cfg(unix)]
    {
        let signals = match oakdb_server::signal::Signals::block() {
            Ok(signals) => signals,
            Err(e) => return fail(format!("cannot block SIGINT and SIGTERM: {e}")),
        };
        if let Err(e) = signals.spawn(handle.clone()) {
            return fail(format!("cannot start the signal thread: {e}"));
        }
    }

    let db = match Db::open(&data) {
        Ok(db) => db,
        Err(e) => return fail(format!("cannot open {}: {e}", data.display())),
    };
    if handle.requested() {
        // A signal came while the WAL was replayed: close without serving.
        return Ok(match db.close() {
            Ok(()) => {
                eprintln!("oakdb: checkpointed and closed");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("oakdb: the final checkpoint failed: {e}");
                ExitCode::from(1)
            }
        });
    }
    let config = Config::default().with(|c| {
        c.port = port;
        c.allow = allow;
        c.max_connections = max_connections;
        // Zero is no limit; the server reads it so.
        c.idle_in_transaction_timeout = Some(idle_in_transaction);
        c.idle_timeout = Some(idle);
        c.long_transaction_warning = Some(warn_age);
        c.shutdown_grace = grace;
    });
    let server = match Server::bind_with(db, config, handle) {
        Ok(server) => server,
        Err(e) => return fail(format!("cannot start: {e}")),
    };
    println!(
        "oakdb {} listening on {}, data {}",
        env!("CARGO_PKG_VERSION"),
        server.local_addr(),
        data.display()
    );
    let report = server.run();
    eprintln!(
        "oakdb: shut down in {:.1}s: {} connections closed, {} transactions rolled back",
        report.took.as_secs_f64(),
        report.closed,
        report.rolled_back
    );
    Ok(if report.is_clean() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn cli(args: &[String]) -> Result<ExitCode, Usage> {
    let mut host = "127.0.0.1".to_string();
    let mut port = DEFAULT_PORT;
    for (flag, value) in flags(args)? {
        match flag {
            "--host" => host = value.to_string(),
            "--port" => port = number(flag, value)?,
            _ => return Err(Usage(format!("unknown flag {flag} for cli"))),
        }
    }
    let mut client = match Client::connect((host.as_str(), port)) {
        Ok(client) => client,
        Err(e) => return fail(format!("cannot connect to {host}:{port}: {e}")),
    };
    let stdin = io::stdin();
    let interactive = stdin.is_terminal();
    if interactive {
        println!(
            "connected to {host}:{port} (oakdb protocol {})",
            oakdb_server::protocol::VERSION
        );
    }
    let code = oakdb_server::cli::run(
        &mut client,
        stdin.lock(),
        &mut io::stdout(),
        &mut io::stderr(),
        interactive,
    );
    match code {
        Ok(code) => Ok(ExitCode::from(code)),
        Err(e) => fail(e),
    }
}
