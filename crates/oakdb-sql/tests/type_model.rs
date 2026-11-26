//! The type checker against a model (issue #34, design 0027 E1, E13).
//!
//! Random WHERE conditions over columns, literals (NULL among them),
//! arithmetic, comparisons, IS NULL, AND, OR and NOT are checked three
//! ways:
//!
//! 1. A model type checker of a few lines, written from design 0019
//!    section 3, and Oakdb agree on whether the condition is a type error.
//! 2. Oakdb gives the same answer on an empty table and on a full one: a
//!    type error does not depend on the rows (design 0019 W5, 0027 E1).
//! 3. A condition that passes the check never fails, while rows are read,
//!    with a type error or with `Corrupt` ("a value of another type").

use oakdb::fs::sim::SimFs;
use oakdb::Db;
use oakdb_sql::{execute, SqlError};
use proptest::prelude::*;

/// A type in the model: one of the column types, or a condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ty {
    Integer,
    Text,
    Blob,
    /// A NULL literal: it fits any type.
    Null,
    /// True or false.
    Bool,
}

/// An expression of the model, printed as SQL with every operand in
/// parentheses so the parser reads the tree as it was built.
#[derive(Debug, Clone)]
enum E {
    Column(&'static str, Ty),
    Literal(&'static str, Ty),
    Neg(Box<E>),
    Arith(&'static str, Box<E>, Box<E>),
    Compare(&'static str, Box<E>, Box<E>),
    IsNull(Box<E>, bool),
    Not(Box<E>),
    And(Box<E>, Box<E>),
    Or(Box<E>, Box<E>),
}

impl E {
    fn sql(&self) -> String {
        match self {
            E::Column(name, _) => (*name).into(),
            E::Literal(text, _) => (*text).into(),
            E::Neg(inner) => format!("-({})", inner.sql()),
            E::Arith(op, l, r) | E::Compare(op, l, r) => {
                format!("({}) {op} ({})", l.sql(), r.sql())
            }
            E::IsNull(inner, negated) => format!(
                "({}) IS {}NULL",
                inner.sql(),
                if *negated { "NOT " } else { "" }
            ),
            E::Not(inner) => format!("NOT ({})", inner.sql()),
            E::And(l, r) => format!("({}) AND ({})", l.sql(), r.sql()),
            E::Or(l, r) => format!("({}) OR ({})", l.sql(), r.sql()),
        }
    }
}

/// The model type checker: the type of `e`, or `None` for a type error.
fn check(e: &E) -> Option<Ty> {
    let value = |e: &E| check(e).filter(|&t| t != Ty::Bool);
    let condition = |e: &E| check(e).filter(|&t| t == Ty::Bool);
    let integer = |e: &E| value(e).filter(|&t| t == Ty::Integer || t == Ty::Null);
    match e {
        E::Column(_, t) | E::Literal(_, t) => Some(*t),
        E::Neg(inner) => integer(inner).map(|_| Ty::Integer),
        E::Arith(_, l, r) => {
            integer(l)?;
            integer(r)?;
            Some(Ty::Integer)
        }
        E::Compare(_, l, r) => {
            let (l, r) = (value(l)?, value(r)?);
            (l == r || l == Ty::Null || r == Ty::Null).then_some(Ty::Bool)
        }
        E::IsNull(inner, _) => value(inner).map(|_| Ty::Bool),
        E::Not(inner) => condition(inner),
        E::And(l, r) | E::Or(l, r) => {
            condition(l)?;
            condition(r)
        }
    }
}

fn leaf() -> impl Strategy<Value = E> {
    prop_oneof![
        Just(E::Column("id", Ty::Integer)),
        Just(E::Column("n", Ty::Integer)),
        Just(E::Column("s", Ty::Text)),
        Just(E::Column("b", Ty::Blob)),
        Just(E::Literal("0", Ty::Integer)),
        Just(E::Literal("2", Ty::Integer)),
        Just(E::Literal("9223372036854775807", Ty::Integer)),
        Just(E::Literal("'a'", Ty::Text)),
        Just(E::Literal("''", Ty::Text)),
        Just(E::Literal("X'00'", Ty::Blob)),
        Just(E::Literal("NULL", Ty::Null)),
    ]
}

fn expr() -> impl Strategy<Value = E> {
    leaf().prop_recursive(5, 40, 2, |inner| {
        let two = (inner.clone(), inner.clone());
        prop_oneof![
            inner.clone().prop_map(|e| E::Neg(Box::new(e))),
            (
                prop::sample::select(vec!["+", "-", "*", "/", "%"]),
                two.clone()
            )
                .prop_map(|(op, (l, r))| E::Arith(op, Box::new(l), Box::new(r))),
            (
                prop::sample::select(vec!["=", "<>", "<", "<=", ">", ">="]),
                two.clone()
            )
                .prop_map(|(op, (l, r))| E::Compare(op, Box::new(l), Box::new(r))),
            (inner.clone(), any::<bool>()).prop_map(|(e, n)| E::IsNull(Box::new(e), n)),
            inner.clone().prop_map(|e| E::Not(Box::new(e))),
            two.clone()
                .prop_map(|(l, r)| E::And(Box::new(l), Box::new(r))),
            two.prop_map(|(l, r)| E::Or(Box::new(l), Box::new(r))),
        ]
    })
}

const CREATE: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER, s TEXT, b BLOB)";

fn empty() -> Db<SimFs> {
    let db = Db::open_with(SimFs::new(), "/oak").unwrap();
    execute(&db, CREATE).unwrap();
    db
}

/// Rows of every shape: small and extreme numbers, NULLs in each column
/// but the key, empty text and blobs.
fn full() -> Db<SimFs> {
    let db = empty();
    execute(
        &db,
        "INSERT INTO t VALUES \
         (-9223372036854775808, 0, 'a', X'00'), \
         (0, NULL, '', X''), \
         (1, 2, NULL, NULL), \
         (2, -1, 'b', X'ff'), \
         (9223372036854775807, 9223372036854775807, 'zz', NULL)",
    )
    .unwrap();
    db
}

/// Whether `e` is the planner's type error.
fn is_type_error(e: &SqlError) -> bool {
    matches!(e, SqlError::Type(_) | SqlError::TypeMismatch { .. })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    #[test]
    fn oakdb_and_the_model_agree_on_type_errors_on_empty_and_full_tables(e in expr()) {
        let sql = format!("SELECT * FROM t WHERE {}", e.sql());
        let model_ok = check(&e) == Some(Ty::Bool);
        let on_empty = execute(&empty(), &sql);
        let on_full = execute(&full(), &sql);
        let empty_type_error = matches!(&on_empty, Err(e) if is_type_error(e));
        let full_type_error = matches!(&on_full, Err(e) if is_type_error(e));
        // 1. The model and Oakdb agree.
        prop_assert_eq!(!model_ok, empty_type_error, "{}: {:?}", sql, on_empty);
        // 2. The rows do not change a type error.
        prop_assert_eq!(empty_type_error, full_type_error, "{}: {:?} / {:?}", sql, on_empty, on_full);
        if model_ok {
            // On an empty table nothing is computed, so nothing fails.
            prop_assert!(on_empty.is_ok(), "{}: {:?}", sql, on_empty);
            // 3. Reading rows fails, if at all, only on a value out of
            // range: never on a type, never as stored data of another
            // type.
            match &on_full {
                Ok(_) | Err(SqlError::Arithmetic(_)) => {}
                Err(other) => prop_assert!(false, "{}: {:?}", sql, other),
            }
        }
    }
}

/// The same check over UPDATE's SET, whose expression must fit its column:
/// a type error or not is the same on an empty and a full table.
#[test]
fn a_set_type_error_does_not_depend_on_the_rows() {
    for (set, type_error) in [
        ("n = n + 1", false),
        ("n = s", true),
        ("s = n", true),
        ("s = NULL", false),
        ("b = X'01'", false),
        ("b = 'a'", true),
        ("n = -s", true),
        ("n = (n = 1)", true),
        ("id = 'a'", true),
    ] {
        let sql = format!("UPDATE t SET {set}");
        let on_empty = execute(&empty(), &sql);
        let on_full = execute(&full(), &sql);
        for result in [&on_empty, &on_full] {
            let got = matches!(result, Err(e) if is_type_error(e));
            assert_eq!(got, type_error, "{sql}: {result:?}");
        }
    }
}
