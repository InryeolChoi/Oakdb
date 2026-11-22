//! Running the `oakdb` executable from tests with a deadline, so a bug that
//! keeps a child process running fails the test instead of hanging it.

// Each test file uses the helpers it needs.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// How long a child process may take.
pub const DEADLINE: Duration = Duration::from_secs(30);

/// Kills a child process by id: the child itself has moved to the thread
/// that waits on it.
fn kill(pid: u32) {
    // SAFETY: kill only sends a signal; a stale pid at worst hits nothing.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

/// Runs `command` with `input` on its standard input and waits for it to
/// exit, at most [`DEADLINE`]; past that it is killed and the test fails.
pub fn output_within(command: &mut Command, input: &str) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_string();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        // A child that exits early closes the pipe; that is its business.
        let _ = stdin.write_all(input.as_bytes());
        drop(stdin);
        tx.send(child.wait_with_output()).ok();
    });
    match rx.recv_timeout(DEADLINE) {
        Ok(out) => out.unwrap(),
        Err(_) => {
            kill(pid);
            panic!("{command:?} did not exit within {DEADLINE:?}");
        }
    }
}

/// Reads the first line `child` prints, at most [`DEADLINE`], and keeps
/// draining its standard output after that so it never blocks on the
/// pipe. Kills the child and fails the test past the deadline.
pub fn first_line(child: &mut Child) -> String {
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut line = String::new();
        let _ = out.read_line(&mut line);
        tx.send(line).ok();
        let _ = std::io::copy(&mut out, &mut std::io::sink());
    });
    match rx.recv_timeout(DEADLINE) {
        Ok(line) => line,
        Err(_) => {
            let _ = child.kill();
            panic!("the child printed no line within {DEADLINE:?}");
        }
    }
}
