//! WHERE checked against a model (issue #32, design 0019 section 8), the
//! way the B+Tree was checked against a `BTreeMap`: random rows, random
//! conditions written out as SQL, and a naive model that reads every row
//! and filters it with its own three-valued evaluator.
//!
//! The model computes every part of every condition on every row, with no
//! shortcuts. So when the model finishes without an arithmetic error,
//! Oakdb, which reads only the rows its range holds and stops early in
//! AND, OR and IN, must not fail either, and must return the same rows.
//! When the model does fail, Oakdb may fail with an arithmetic error or
//! succeed (design 0019, section 2-2).

use std::collections::BTreeMap;

use oakdb::fs::sim::SimFs;
use oakdb::Db;
use oakdb_sql::{execute, Output, SqlError, Value};
use proptest::prelude::*;

// ------------------------------------------------------------ the model

/// An integer expression. `Col` indexes the table's integer columns.
#[derive(Debug, Clone)]
enum I {
    Col(usize),
    Lit(Option<i64>),
    Neg(Box<I>),
    Bin(&'static str, Box<I>, Box<I>),
}

/// A text expression. `Col` indexes the table's text columns.
#[derive(Debug, Clone)]
enum T {
    Col(usize),
    Lit(Option<String>),
}

/// A condition.
#[derive(Debug, Clone)]
enum B {
    CmpI(&'static str, I, I),
    CmpT(&'static str, T, T),
    IsNullI(I, bool),
    IsNullT(T, bool),
    BetweenI(I, I, I, bool),
    BetweenT(T, T, T, bool),
    InI(I, Vec<I>, bool),
    InT(T, Vec<T>, bool),
    Not(Box<B>),
    And(Box<B>, Box<B>),
    Or(Box<B>, Box<B>),
}

/// The table: its name, integer and text columns, and whether the primary
/// key is integer column 0 (`true`) or text column 0 (`false`).
struct Schema {
    table: &'static str,
    ints: &'static [&'static str],
    texts: &'static [&'static str],
    int_key: bool,
}

#[derive(Debug, Clone)]
struct Row {
    ints: Vec<Option<i64>>,
    texts: Vec<Option<String>>,
}

/// An arithmetic error in the model.
#[derive(Debug)]
struct Overflow;

fn eval_i(e: &I, row: &Row) -> Result<Option<i64>, Overflow> {
    Ok(match e {
        I::Col(i) => row.ints[*i],
        I::Lit(v) => *v,
        I::Neg(e) => match eval_i(e, row)? {
            None => None,
            Some(v) => Some(v.checked_neg().ok_or(Overflow)?),
        },
        I::Bin(op, l, r) => {
            let (l, r) = (eval_i(l, row)?, eval_i(r, row)?);
            match (l, r) {
                (Some(l), Some(r)) => Some(
                    match *op {
                        "+" => l.checked_add(r),
                        "-" => l.checked_sub(r),
                        "*" => l.checked_mul(r),
                        "/" => l.checked_div(r),
                        "%" => l.checked_rem(r),
                        _ => unreachable!(),
                    }
                    .ok_or(Overflow)?,
                ),
                _ => None,
            }
        }
    })
}

fn eval_t(e: &T, row: &Row) -> Option<String> {
    match e {
        T::Col(i) => row.texts[*i].clone(),
        T::Lit(v) => v.clone(),
    }
}

fn cmp<V: Ord>(op: &str, l: Option<V>, r: Option<V>) -> Option<bool> {
    let (l, r) = (l?, r?);
    Some(match op {
        "=" => l == r,
        "!=" | "<>" => l != r,
        "<" => l < r,
        "<=" => l <= r,
        ">" => l > r,
        ">=" => l >= r,
        _ => unreachable!(),
    })
}

fn and(l: Option<bool>, r: Option<bool>) -> Option<bool> {
    match (l, r) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

fn or(l: Option<bool>, r: Option<bool>) -> Option<bool> {
    match (l, r) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

fn not(v: Option<bool>, negate: bool) -> Option<bool> {
    if negate {
        v.map(|b| !b)
    } else {
        v
    }
}

fn between<V: Ord + Clone>(x: Option<V>, lo: Option<V>, hi: Option<V>) -> Option<bool> {
    and(cmp(">=", x.clone(), lo), cmp("<=", x, hi))
}

fn member<V: Ord>(x: Option<V>, list: Vec<Option<V>>) -> Option<bool> {
    let x = x?;
    let mut unknown = false;
    for item in list {
        match item {
            Some(item) if item == x => return Some(true),
            Some(_) => {}
            None => unknown = true,
        }
    }
    if unknown {
        None
    } else {
        Some(false)
    }
}

fn eval_b(e: &B, row: &Row) -> Result<Option<bool>, Overflow> {
    Ok(match e {
        B::CmpI(op, l, r) => cmp(op, eval_i(l, row)?, eval_i(r, row)?),
        B::CmpT(op, l, r) => cmp(op, eval_t(l, row), eval_t(r, row)),
        B::IsNullI(e, negated) => Some(eval_i(e, row)?.is_none() != *negated),
        B::IsNullT(e, negated) => Some(eval_t(e, row).is_none() != *negated),
        B::BetweenI(x, lo, hi, negated) => not(
            between(eval_i(x, row)?, eval_i(lo, row)?, eval_i(hi, row)?),
            *negated,
        ),
        B::BetweenT(x, lo, hi, negated) => not(
            between(eval_t(x, row), eval_t(lo, row), eval_t(hi, row)),
            *negated,
        ),
        B::InI(x, list, negated) => {
            let x = eval_i(x, row)?;
            let list = list
                .iter()
                .map(|e| eval_i(e, row))
                .collect::<Result<Vec<_>, _>>()?;
            not(member(x, list), *negated)
        }
        B::InT(x, list, negated) => not(
            member(
                eval_t(x, row),
                list.iter().map(|e| eval_t(e, row)).collect(),
            ),
            *negated,
        ),
        B::Not(e) => not(eval_b(e, row)?, true),
        B::And(l, r) => and(eval_b(l, row)?, eval_b(r, row)?),
        B::Or(l, r) => or(eval_b(l, row)?, eval_b(r, row)?),
    })
}

// ------------------------------------------------------------ as SQL

fn sql_i(e: &I, s: &Schema) -> String {
    match e {
        I::Col(i) => s.ints[*i].to_owned(),
        I::Lit(None) => "NULL".to_owned(),
        I::Lit(Some(v)) => v.to_string(),
        I::Neg(e) => format!("-({})", sql_i(e, s)),
        I::Bin(op, l, r) => format!("({} {op} {})", sql_i(l, s), sql_i(r, s)),
    }
}

fn sql_t(e: &T, s: &Schema) -> String {
    match e {
        T::Col(i) => s.texts[*i].to_owned(),
        T::Lit(None) => "NULL".to_owned(),
        T::Lit(Some(v)) => format!("'{}'", v.replace('\'', "''")),
    }
}

fn negation(negated: bool) -> &'static str {
    if negated {
        "NOT "
    } else {
        ""
    }
}

fn sql_b(e: &B, s: &Schema) -> String {
    let list_i = |list: &[I]| {
        list.iter()
            .map(|e| sql_i(e, s))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let list_t = |list: &[T]| {
        list.iter()
            .map(|e| sql_t(e, s))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match e {
        B::CmpI(op, l, r) => format!("({} {op} {})", sql_i(l, s), sql_i(r, s)),
        B::CmpT(op, l, r) => format!("({} {op} {})", sql_t(l, s), sql_t(r, s)),
        B::IsNullI(e, n) => format!("({} IS {}NULL)", sql_i(e, s), negation(*n)),
        B::IsNullT(e, n) => format!("({} IS {}NULL)", sql_t(e, s), negation(*n)),
        B::BetweenI(x, lo, hi, n) => format!(
            "({} {}BETWEEN {} AND {})",
            sql_i(x, s),
            negation(*n),
            sql_i(lo, s),
            sql_i(hi, s)
        ),
        B::BetweenT(x, lo, hi, n) => format!(
            "({} {}BETWEEN {} AND {})",
            sql_t(x, s),
            negation(*n),
            sql_t(lo, s),
            sql_t(hi, s)
        ),
        B::InI(x, list, n) => format!("({} {}IN ({}))", sql_i(x, s), negation(*n), list_i(list)),
        B::InT(x, list, n) => format!("({} {}IN ({}))", sql_t(x, s), negation(*n), list_t(list)),
        B::Not(e) => format!("(NOT {})", sql_b(e, s)),
        B::And(l, r) => format!("({} AND {})", sql_b(l, s), sql_b(r, s)),
        B::Or(l, r) => format!("({} OR {})", sql_b(l, s), sql_b(r, s)),
    }
}

// ------------------------------------------------------------ generators

fn int_value() -> impl Strategy<Value = i64> {
    prop_oneof![
        8 => -4i64..=4,
        1 => prop::sample::select(vec![i64::MIN, i64::MIN + 1, i64::MAX - 1, i64::MAX, 100, -100]),
    ]
}

fn text_value() -> impl Strategy<Value = String> {
    // "a\0" and "a\0b" sit between "a" and "ab" (review P2).
    prop::sample::select(vec![
        "", "a", "a\0", "a\0b", "ab", "abc", "b", "B", "é", "it's",
    ])
    .prop_map(str::to_owned)
}

/// An integer expression over `columns` integer columns; with none, a
/// constant.
fn int_expr(columns: usize) -> BoxedStrategy<I> {
    let literal = prop::option::weighted(0.85, int_value()).prop_map(I::Lit);
    let leaf = if columns == 0 {
        literal.boxed()
    } else {
        prop_oneof![3 => (0..columns).prop_map(I::Col), 3 => literal].boxed()
    };
    leaf.prop_recursive(2, 6, 2, |inner| {
        prop_oneof![
            1 => inner.clone().prop_map(|e| I::Neg(Box::new(e))),
            4 => (
                prop::sample::select(vec!["+", "-", "*", "/", "%"]),
                inner.clone(),
                inner
            )
                .prop_map(|(op, l, r)| I::Bin(op, Box::new(l), Box::new(r))),
        ]
    })
    .boxed()
}

fn text_expr(columns: usize) -> impl Strategy<Value = T> {
    prop_oneof![
        1 => (0..columns).prop_map(T::Col),
        1 => prop::option::weighted(0.85, text_value()).prop_map(T::Lit),
    ]
}

fn compare_op() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec!["=", "!=", "<>", "<", "<=", ">", ">="])
}

fn condition(s: &Schema) -> impl Strategy<Value = B> {
    let (ints, texts) = (s.ints.len(), s.texts.len());
    // Conditions on the bare primary key against a constant, the kind
    // the planner turns into a range, come often.
    let key =
        if s.int_key {
            prop_oneof![
                (compare_op(), int_expr(0), any::<bool>()).prop_map(|(op, c, flip)| if flip {
                    B::CmpI(op, c, I::Col(0))
                } else {
                    B::CmpI(op, I::Col(0), c)
                }),
                (int_value(), int_value(), any::<bool>()).prop_map(|(lo, hi, n)| B::BetweenI(
                    I::Col(0),
                    I::Lit(Some(lo)),
                    I::Lit(Some(hi)),
                    n
                )),
                (
                    prop::collection::vec(prop::option::weighted(0.9, int_value()), 1..4),
                    any::<bool>()
                )
                    .prop_map(|(list, n)| B::InI(
                        I::Col(0),
                        list.into_iter().map(I::Lit).collect(),
                        n
                    )),
            ]
            .boxed()
        } else {
            prop_oneof![
                (compare_op(), prop::option::weighted(0.9, text_value()))
                    .prop_map(|(op, c)| B::CmpT(op, T::Col(0), T::Lit(c))),
                (text_value(), text_value(), any::<bool>()).prop_map(|(lo, hi, n)| B::BetweenT(
                    T::Col(0),
                    T::Lit(Some(lo)),
                    T::Lit(Some(hi)),
                    n
                )),
                (
                    prop::collection::vec(prop::option::weighted(0.9, text_value()), 1..4),
                    any::<bool>()
                )
                    .prop_map(|(list, n)| B::InT(
                        T::Col(0),
                        list.into_iter().map(T::Lit).collect(),
                        n
                    )),
            ]
            .boxed()
        };
    let leaf = prop_oneof![
        4 => key,
        3 => (compare_op(), int_expr(ints), int_expr(ints)).prop_map(|(op, l, r)| B::CmpI(op, l, r)),
        2 => (compare_op(), text_expr(texts), text_expr(texts)).prop_map(|(op, l, r)| B::CmpT(op, l, r)),
        1 => (int_expr(ints), any::<bool>()).prop_map(|(e, n)| B::IsNullI(e, n)),
        1 => (text_expr(texts), any::<bool>()).prop_map(|(e, n)| B::IsNullT(e, n)),
        1 => (int_expr(ints), int_expr(ints), int_expr(ints), any::<bool>())
            .prop_map(|(x, lo, hi, n)| B::BetweenI(x, lo, hi, n)),
        1 => (text_expr(texts), text_expr(texts), text_expr(texts), any::<bool>())
            .prop_map(|(x, lo, hi, n)| B::BetweenT(x, lo, hi, n)),
        1 => (int_expr(ints), prop::collection::vec(int_expr(ints), 1..4), any::<bool>())
            .prop_map(|(x, list, n)| B::InI(x, list, n)),
        1 => (text_expr(texts), prop::collection::vec(text_expr(texts), 1..4), any::<bool>())
            .prop_map(|(x, list, n)| B::InT(x, list, n)),
    ];
    leaf.prop_recursive(3, 12, 2, |inner| {
        prop_oneof![
            1 => inner.clone().prop_map(|e| B::Not(Box::new(e))),
            3 => (inner.clone(), inner.clone()).prop_map(|(l, r)| B::And(Box::new(l), Box::new(r))),
            2 => (inner.clone(), inner).prop_map(|(l, r)| B::Or(Box::new(l), Box::new(r))),
        ]
    })
}

fn rows(s: &Schema) -> impl Strategy<Value = Vec<Row>> {
    let row = (
        prop::collection::vec(prop::option::weighted(0.8, int_value()), s.ints.len()),
        prop::collection::vec(prop::option::weighted(0.8, text_value()), s.texts.len()),
    )
        .prop_map(|(ints, texts)| Row { ints, texts });
    prop::collection::vec(row, 0..24)
}

// ------------------------------------------------------------ the check

/// The value a row's primary key holds, as Oakdb returns it.
fn key_of(s: &Schema, row: &Row) -> Option<Value> {
    if s.int_key {
        row.ints[0].map(Value::Integer)
    } else {
        row.texts[0].clone().map(Value::Text)
    }
}

/// Sorts values of one type the way the table sorts them.
fn sort(values: &mut [Value]) {
    values.sort_by(|a, b| match (a, b) {
        (Value::Integer(a), Value::Integer(b)) => a.cmp(b),
        (Value::Text(a), Value::Text(b)) => a.cmp(b),
        _ => unreachable!("one primary key type per table"),
    });
}

fn check(s: &Schema, generated: Vec<Row>, conditions: Vec<B>) -> Result<(), TestCaseError> {
    let db = Db::open_with(SimFs::new(), "/oak").unwrap();
    let columns: Vec<String> = s
        .ints
        .iter()
        .map(|c| format!("{c} INTEGER"))
        .chain(s.texts.iter().map(|c| format!("{c} TEXT")))
        .enumerate()
        .map(|(i, c)| {
            let key = (s.int_key && i == 0) || (!s.int_key && i == s.ints.len());
            if key {
                format!("{c} PRIMARY KEY")
            } else {
                c
            }
        })
        .collect();
    execute(
        &db,
        &format!("CREATE TABLE {} ({})", s.table, columns.join(", ")),
    )
    .unwrap();
    // The model: one row per primary key, the last one generated wins.
    let mut model: BTreeMap<String, Row> = BTreeMap::new();
    for row in generated {
        if let Some(key) = key_of(s, &row) {
            model.insert(format!("{key:?}"), row);
        }
    }
    let names: Vec<&str> = s.ints.iter().chain(s.texts).copied().collect();
    for row in model.values() {
        let values: Vec<String> = row
            .ints
            .iter()
            .map(|v| sql_i(&I::Lit(*v), s))
            .chain(row.texts.iter().map(|v| sql_t(&T::Lit(v.clone()), s)))
            .collect();
        execute(
            &db,
            &format!(
                "INSERT INTO {} ({}) VALUES ({})",
                s.table,
                names.join(", "),
                values.join(", ")
            ),
        )
        .unwrap();
    }
    let key_column = if s.int_key { s.ints[0] } else { s.texts[0] };
    for condition in conditions {
        let sql = format!(
            "SELECT {key_column} FROM {} WHERE {}",
            s.table,
            sql_b(&condition, s)
        );
        let mut expected = Ok(Vec::new());
        for row in model.values() {
            match eval_b(&condition, row) {
                Err(e) => {
                    expected = Err(e);
                    break;
                }
                Ok(Some(true)) => {
                    if let Ok(keys) = &mut expected {
                        keys.extend(key_of(s, row));
                    }
                }
                Ok(_) => {}
            }
        }
        let got = execute(&db, &sql);
        match (expected, got) {
            (Ok(mut expected), Ok(Output::Rows { rows, .. })) => {
                let mut got: Vec<Value> = rows.into_iter().map(|mut r| r.remove(0)).collect();
                sort(&mut expected);
                sort(&mut got);
                prop_assert_eq!(got, expected, "{}", sql);
            }
            (Ok(_), got) => prop_assert!(false, "{sql}: the model succeeded, Oakdb gave {got:?}"),
            (Err(Overflow), Ok(_) | Err(SqlError::Arithmetic(_))) => {}
            (Err(Overflow), Err(e)) => {
                prop_assert!(false, "{sql}: the model overflowed, Oakdb gave {e:?}")
            }
        }
    }
    Ok(())
}

const INT_KEYED: Schema = Schema {
    table: "t",
    ints: &["id", "a"],
    texts: &["b"],
    int_key: true,
};

const TEXT_KEYED: Schema = Schema {
    table: "s",
    ints: &["n"],
    texts: &["k", "c"],
    int_key: false,
};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn where_on_an_integer_key_matches_the_model(
        rows in rows(&INT_KEYED),
        conditions in prop::collection::vec(condition(&INT_KEYED), 1..8),
    ) {
        check(&INT_KEYED, rows, conditions)?;
    }

    #[test]
    fn where_on_a_text_key_matches_the_model(
        rows in rows(&TEXT_KEYED),
        conditions in prop::collection::vec(condition(&TEXT_KEYED), 1..8),
    ) {
        check(&TEXT_KEYED, rows, conditions)?;
    }
}

// ------------------------------------------------------------ UPDATE and DELETE

// Issue #33, design 0022 section 8: the same model, now changed by runs of
// INSERT, UPDATE and DELETE. After every statement the row count or the
// error and the whole table must match the model. The model applies an
// UPDATE to the whole set of matched rows at once: every SET reads the old
// row, and a duplicate key is one in the table after the statement.

/// A statement that changes the table.
#[derive(Debug, Clone)]
enum Write {
    /// SET on integer and text columns, by column index, then WHERE.
    Update(Vec<(usize, I)>, Vec<(usize, T)>, Option<B>),
    Delete(Option<B>),
    Insert(Row),
}

/// What made a statement fail, in the model and in Oakdb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    Overflow,
    NullKey,
    Duplicate,
}

/// A row's primary key, comparable within one table.
type Key = (Option<i64>, Option<String>);

type Model = BTreeMap<Key, Row>;

/// What the model says a statement does.
enum Expect {
    /// The row count and the table after it, or the ways it may fail.
    Strict(Result<(u64, Model), Vec<Failure>>),
    /// WHERE overflowed on some row, which Oakdb may never read (design
    /// 0019, section 2-2): it may fail, or succeed.
    Loose,
}

fn key_in(s: &Schema, row: &Row) -> Option<Key> {
    if s.int_key {
        row.ints[0].map(|k| (Some(k), None))
    } else {
        row.texts[0].clone().map(|k| (None, Some(k)))
    }
}

/// The keys of the rows `filter` makes true, computing it on every row
/// with no shortcut, so that an overflow anywhere shows.
fn matching(model: &Model, filter: &Option<B>) -> Result<Vec<Key>, Overflow> {
    let mut keys = Vec::new();
    let mut overflow = false;
    for (key, row) in model {
        match filter.as_ref().map_or(Ok(Some(true)), |b| eval_b(b, row)) {
            Err(Overflow) => overflow = true,
            Ok(Some(true)) => keys.push(key.clone()),
            Ok(_) => {}
        }
    }
    if overflow {
        Err(Overflow)
    } else {
        Ok(keys)
    }
}

fn expect(s: &Schema, model: &Model, write: &Write) -> Expect {
    match write {
        Write::Insert(row) => Expect::Strict(match key_in(s, row) {
            None => Err(vec![Failure::NullKey]),
            Some(key) if model.contains_key(&key) => Err(vec![Failure::Duplicate]),
            Some(key) => {
                let mut next = model.clone();
                next.insert(key, row.clone());
                Ok((1, next))
            }
        }),
        Write::Delete(filter) => match matching(model, filter) {
            Err(Overflow) => Expect::Loose,
            Ok(keys) => {
                let mut next = model.clone();
                for key in &keys {
                    next.remove(key);
                }
                Expect::Strict(Ok((keys.len() as u64, next)))
            }
        },
        Write::Update(ints, texts, filter) => {
            let Ok(keys) = matching(model, filter) else {
                return Expect::Loose;
            };
            // Every SET reads the old row.
            let mut failures = Vec::new();
            let mut changed = Vec::new();
            for key in &keys {
                let old = &model[key];
                let mut new = old.clone();
                for (i, e) in ints {
                    match eval_i(e, old) {
                        Ok(v) => new.ints[*i] = v,
                        Err(Overflow) => failures.push(Failure::Overflow),
                    }
                }
                for (i, e) in texts {
                    new.texts[*i] = eval_t(e, old);
                }
                match key_in(s, &new) {
                    Some(key) => changed.push((key, new)),
                    None => failures.push(Failure::NullKey),
                }
            }
            if !failures.is_empty() {
                return Expect::Strict(Err(failures));
            }
            // A duplicate is a key twice in the table after the statement.
            let mut next = model.clone();
            for key in &keys {
                next.remove(key);
            }
            for (key, row) in changed {
                if next.insert(key, row).is_some() {
                    return Expect::Strict(Err(vec![Failure::Duplicate]));
                }
            }
            Expect::Strict(Ok((keys.len() as u64, next)))
        }
    }
}

fn write_sql(s: &Schema, write: &Write) -> String {
    let filter = |f: &Option<B>| {
        f.as_ref()
            .map_or(String::new(), |b| format!(" WHERE {}", sql_b(b, s)))
    };
    match write {
        Write::Update(ints, texts, f) => {
            let sets: Vec<String> = ints
                .iter()
                .map(|(i, e)| format!("{} = {}", s.ints[*i], sql_i(e, s)))
                .chain(
                    texts
                        .iter()
                        .map(|(i, e)| format!("{} = {}", s.texts[*i], sql_t(e, s))),
                )
                .collect();
            format!("UPDATE {} SET {}{}", s.table, sets.join(", "), filter(f))
        }
        Write::Delete(f) => format!("DELETE FROM {}{}", s.table, filter(f)),
        Write::Insert(row) => {
            let names: Vec<&str> = s.ints.iter().chain(s.texts).copied().collect();
            let values: Vec<String> = row
                .ints
                .iter()
                .map(|v| sql_i(&I::Lit(*v), s))
                .chain(row.texts.iter().map(|v| sql_t(&T::Lit(v.clone()), s)))
                .collect();
            format!(
                "INSERT INTO {} ({}) VALUES ({})",
                s.table,
                names.join(", "),
                values.join(", ")
            )
        }
    }
}

/// Oakdb's whole table, as the model holds it.
fn table_of(s: &Schema, db: &Db<SimFs>) -> Model {
    let Ok(Output::Rows { rows, .. }) = execute(db, &format!("SELECT * FROM {}", s.table)) else {
        panic!("SELECT * failed");
    };
    let mut table = Model::new();
    for values in rows {
        let (ints, texts) = values.split_at(s.ints.len());
        let row = Row {
            ints: ints
                .iter()
                .map(|v| match v {
                    Value::Null => None,
                    Value::Integer(i) => Some(*i),
                    other => panic!("{other:?}"),
                })
                .collect(),
            texts: texts
                .iter()
                .map(|v| match v {
                    Value::Null => None,
                    Value::Text(t) => Some(t.clone()),
                    other => panic!("{other:?}"),
                })
                .collect(),
        };
        table.insert(key_in(s, &row).expect("no NULL key is stored"), row);
    }
    table
}

fn same(a: &Model, b: &Model) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|((ka, ra), (kb, rb))| ka == kb && ra.ints == rb.ints && ra.texts == rb.texts)
}

fn failure(e: &SqlError) -> Option<Failure> {
    match e {
        SqlError::Arithmetic(_) => Some(Failure::Overflow),
        SqlError::TypeMismatch { found: None, .. } => Some(Failure::NullKey),
        SqlError::DuplicateKey { .. } => Some(Failure::Duplicate),
        _ => None,
    }
}

fn check_writes(s: &Schema, generated: Vec<Row>, writes: Vec<Write>) -> Result<(), TestCaseError> {
    let db = Db::open_with(SimFs::new(), "/oak").unwrap();
    let columns: Vec<String> = s
        .ints
        .iter()
        .map(|c| format!("{c} INTEGER"))
        .chain(s.texts.iter().map(|c| format!("{c} TEXT")))
        .enumerate()
        .map(|(i, c)| {
            let key = (s.int_key && i == 0) || (!s.int_key && i == s.ints.len());
            if key {
                format!("{c} PRIMARY KEY")
            } else {
                c
            }
        })
        .collect();
    execute(
        &db,
        &format!("CREATE TABLE {} ({})", s.table, columns.join(", ")),
    )
    .unwrap();
    let mut model = Model::new();
    for row in generated {
        if let Some(key) = key_in(s, &row) {
            model.entry(key).or_insert_with(|| {
                execute(&db, &write_sql(s, &Write::Insert(row.clone()))).unwrap();
                row
            });
        }
    }
    for write in &writes {
        let sql = write_sql(s, write);
        let expected = expect(s, &model, write);
        let got = execute(&db, &sql);
        let table = table_of(s, &db);
        match (expected, got) {
            (Expect::Strict(Ok((n, next))), Ok(Output::Affected(count))) => {
                prop_assert_eq!(count, n, "{}", sql);
                prop_assert!(
                    same(&table, &next),
                    "{sql}: table {table:?}, model {next:?}"
                );
                model = next;
            }
            (Expect::Strict(Err(may)), Err(e)) => {
                prop_assert!(
                    failure(&e).is_some_and(|f| may.contains(&f)),
                    "{sql}: the model expected one of {may:?}, Oakdb gave {e:?}"
                );
                prop_assert!(
                    same(&table, &model),
                    "{sql}: a failed statement changed the table"
                );
            }
            (Expect::Loose, Ok(Output::Affected(_))) => model = table,
            (Expect::Loose, Err(e)) => {
                prop_assert!(failure(&e).is_some(), "{sql}: {e:?}");
                prop_assert!(
                    same(&table, &model),
                    "{sql}: a failed statement changed the table"
                );
            }
            (Expect::Strict(want), got) => prop_assert!(
                false,
                "{sql}: the model expected {:?}, Oakdb gave {got:?}",
                want.map(|(n, _)| n)
            ),
            (Expect::Loose, got) => prop_assert!(false, "{sql}: {got:?}"),
        }
    }
    Ok(())
}

/// What SET gives an integer column. For the primary key, shifts of
/// neighbouring keys come often, as they meet the duplicate rule of
/// design 0022, section 3-1.
fn int_set(s: &Schema, column: usize) -> BoxedStrategy<I> {
    let ints = s.ints.len();
    if s.int_key && column == 0 {
        let shift =
            |op: &'static str, k: i64| I::Bin(op, Box::new(I::Col(0)), Box::new(I::Lit(Some(k))));
        prop_oneof![
            3 => (-2i64..=2).prop_map(move |k| shift("+", k)),
            1 => Just(shift("*", 0)),
            1 => prop::option::weighted(0.9, int_value()).prop_map(I::Lit),
            1 => (1..ints).prop_map(I::Col),
            1 => int_expr(ints),
        ]
        .boxed()
    } else {
        int_expr(ints)
    }
}

fn text_set(s: &Schema) -> BoxedStrategy<T> {
    let texts = s.texts.len();
    prop_oneof![
        1 => (0..texts).prop_map(T::Col),
        1 => prop::option::weighted(0.9, text_value()).prop_map(T::Lit),
    ]
    .boxed()
}

fn writes(s: &'static Schema) -> impl Strategy<Value = Vec<Write>> {
    let filter = prop::option::weighted(0.8, condition(s).boxed()).boxed();
    let int_sets: Vec<_> = (0..s.ints.len())
        .map(|i| prop::option::weighted(0.4, int_set(s, i)))
        .collect();
    let text_sets: Vec<_> = (0..s.texts.len())
        .map(|_| prop::option::weighted(0.4, text_set(s)))
        .collect();
    let update = (int_sets, text_sets, filter.clone())
        .prop_filter("SET needs a column", |(i, t, _)| {
            i.iter().any(Option::is_some) || t.iter().any(Option::is_some)
        })
        .prop_map(|(i, t, f)| {
            Write::Update(
                i.into_iter()
                    .enumerate()
                    .filter_map(|(c, e)| e.map(|e| (c, e)))
                    .collect(),
                t.into_iter()
                    .enumerate()
                    .filter_map(|(c, e)| e.map(|e| (c, e)))
                    .collect(),
                f,
            )
        });
    let insert = (
        prop::collection::vec(prop::option::weighted(0.9, int_value()), s.ints.len()),
        prop::collection::vec(prop::option::weighted(0.9, text_value()), s.texts.len()),
    )
        .prop_map(|(ints, texts)| Write::Insert(Row { ints, texts }));
    let write = prop_oneof![
        4 => update,
        2 => filter.prop_map(Write::Delete),
        1 => insert,
    ];
    prop::collection::vec(write, 1..8)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn writes_on_an_integer_key_match_the_model(
        rows in rows(&INT_KEYED),
        writes in writes(&INT_KEYED),
    ) {
        check_writes(&INT_KEYED, rows, writes)?;
    }

    #[test]
    fn writes_on_a_text_key_match_the_model(
        rows in rows(&TEXT_KEYED),
        writes in writes(&TEXT_KEYED),
    ) {
        check_writes(&TEXT_KEYED, rows, writes)?;
    }
}
