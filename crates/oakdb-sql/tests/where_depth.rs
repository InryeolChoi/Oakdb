//! Deeply nested WHERE clauses (PR #69 review S1): parsing, planning,
//! computing and dropping an expression all recurse over its tree, so a
//! tree deep enough would overflow the stack and abort the process, which
//! no caller can catch. The parser refuses expressions nested deeper than
//! a limit, so every later step recurses a bounded number of times. These
//! run on a 256 KiB stack, a network server's per-connection stack, and on
//! the default one.

use std::thread;

use oakdb::fs::sim::SimFs;
use oakdb::Db;
use oakdb_sql::{execute, Output, SqlError};

/// The limit the error names.
const LIMIT: &str = "200";

/// A network server's per-connection stack.
const SMALL_STACK: usize = 256 * 1024;

/// Runs `f` on a thread with a stack of `size` bytes.
fn on_stack<T: Send + 'static>(size: usize, f: impl FnOnce() -> T + Send + 'static) -> T {
    thread::Builder::new()
        .stack_size(size)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap()
}

/// Runs `SELECT id FROM t WHERE <condition>` on a table holding ids 1..=3,
/// and returns how many rows it selected.
fn select(condition: String) -> Result<usize, SqlError> {
    let db = Db::open_with(SimFs::new(), "/oak").unwrap();
    execute(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)").unwrap();
    execute(&db, "INSERT INTO t VALUES (1, 1), (2, 2), (3, 3)").unwrap();
    match execute(&db, &format!("SELECT id FROM t WHERE {condition}"))? {
        Output::Rows { rows, .. } => Ok(rows.len()),
        other => panic!("{other:?}"),
    }
}

fn nots(n: usize) -> String {
    format!("{}id = 1", "NOT ".repeat(n))
}

fn parentheses(n: usize) -> String {
    format!("{}id = 1{}", "(".repeat(n), ")".repeat(n))
}

fn plus_chain(n: usize) -> String {
    format!("id{} > 0", " + 1".repeat(n))
}

fn or_chain(n: usize) -> String {
    format!("id = 0{}", " OR id = 0".repeat(n))
}

fn minuses(n: usize) -> String {
    format!("{}id = 1", "- ".repeat(n))
}

fn assert_too_deep(result: Result<usize, SqlError>, what: &str) {
    match result {
        Err(SqlError::Syntax { message, .. }) => {
            assert!(message.contains(LIMIT), "{what}: {message}")
        }
        other => panic!("{what}: expected a syntax error naming the limit, got {other:?}"),
    }
}

#[test]
fn too_deep_expressions_are_syntax_errors_on_a_small_stack() {
    on_stack(SMALL_STACK, || {
        assert_too_deep(select(nots(20_000)), "20,000 NOTs");
        assert_too_deep(select(parentheses(5_000)), "5,000 parentheses");
        assert_too_deep(select(plus_chain(20_000)), "a 20,000-term + chain");
        assert_too_deep(select(or_chain(20_000)), "a 20,000-term OR chain");
        assert_too_deep(select(minuses(20_000)), "20,000 unary minuses");
    });
}

#[test]
fn too_deep_expressions_are_syntax_errors_on_the_default_stack() {
    assert_too_deep(select(nots(20_000)), "20,000 NOTs");
    assert_too_deep(select(parentheses(5_000)), "5,000 parentheses");
    assert_too_deep(select(plus_chain(20_000)), "a 20,000-term + chain");
}

#[test]
fn expressions_just_under_the_limit_run_on_a_small_stack() {
    on_stack(SMALL_STACK, || {
        // NOT 198 times over `id = 1`: an even count keeps it `id = 1`.
        assert_eq!(select(nots(198)).unwrap(), 1);
        assert_eq!(select(parentheses(190)).unwrap(), 1);
        assert_eq!(select(plus_chain(190)).unwrap(), 3);
        assert_eq!(select(or_chain(190)).unwrap(), 0);
        assert_eq!(select(minuses(190)).unwrap(), 1);
    });
}

#[test]
fn a_long_in_list_is_flat_not_deep() {
    let list: Vec<String> = (0..10_000).map(|i| i.to_string()).collect();
    let condition = format!("n IN ({})", list.join(", "));
    assert_eq!(on_stack(SMALL_STACK, move || select(condition)).unwrap(), 3);
}
