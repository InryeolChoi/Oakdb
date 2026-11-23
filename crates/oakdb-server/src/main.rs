//! `oakdb serve` runs the server; `oakdb cli` connects to one (design
//! 0020, section 6).

use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::process::ExitCode;

use oakdb::Db;
use oakdb_server::allow::{Allowlist, Ipv4Net};
use oakdb_server::client::Client;
use oakdb_server::server::{Config, Server, DEFAULT_MAX_CONNECTIONS, DEFAULT_PORT};

const USAGE: &str = "\
usage:
  oakdb serve --data <dir> [--port <n>] [--allow <ip or cidr>]... [--allow-file <file>]
              [--max-connections <n>]
  oakdb cli [--host <host>] [--port <n>]

serve
  --data <dir>             database directory, created if missing (required)
  --port <n>               TCP port, 0 for any free one (default 7878)
  --allow <ip or cidr>     let clients from this IPv4 address or block in; repeatable
  --allow-file <file>      the same, one entry per line, # starts a comment
  --max-connections <n>    connections taken at once (default 512)

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

fn serve(args: &[String]) -> Result<ExitCode, Usage> {
    let mut data: Option<PathBuf> = None;
    let mut port = DEFAULT_PORT;
    let mut max_connections = DEFAULT_MAX_CONNECTIONS;
    let mut nets: Vec<Ipv4Net> = Vec::new();
    for (flag, value) in flags(args)? {
        match flag {
            "--data" => data = Some(PathBuf::from(value)),
            "--port" => port = number(flag, value)?,
            "--max-connections" => max_connections = number(flag, value)?,
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

    let db = match Db::open(&data) {
        Ok(db) => db,
        Err(e) => return fail(format!("cannot open {}: {e}", data.display())),
    };
    let config = Config {
        port,
        allow,
        max_connections,
    };
    let server = match Server::bind(db, config) {
        Ok(server) => server,
        Err(e) => return fail(format!("cannot start: {e}")),
    };
    println!(
        "oakdb {} listening on {}, data {}",
        env!("CARGO_PKG_VERSION"),
        server.local_addr(),
        data.display()
    );
    server.run();
    Ok(ExitCode::SUCCESS)
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
