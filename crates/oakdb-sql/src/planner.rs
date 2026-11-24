//! The planner (issue #32, design 0019, sections 3, 4 and 6): turns a
//! WHERE clause into the range of primary keys to scan and the condition
//! to check on each row read.
//!
//! It knows the table but not the engine, so everything here runs before
//! any row is read: an unknown column or a type error fails the statement
//! without reading. The range only narrows where to look; the whole
//! condition is still checked on every row, so a range wider than needed
//! is slow but never wrong ("generous range, exact filter").
//!
//! The functions that recurse over the tree keep only the recursive cases
//! and hand each leaf to a function of its own, so that their stack frames
//! stay small: the parser bounds the tree's height (PR #69 review S1), and
//! small frames keep that height within a server thread's stack.

use std::cmp::Ordering;
use std::ops::Bound;

use crate::catalog::Table;
use crate::encoding::encode_key;
use crate::expr::{CompareOp, Expr, Pred, Scalar};
use crate::{ColumnType, SqlError, Value};

/// A range of primary key values. `Unbounded` stands for the table's own
/// first or last row.
pub(crate) type KeyRange = (Bound<Value>, Bound<Value>);

/// How to run a WHERE clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    /// The primary keys to scan.
    pub(crate) range: KeyRange,
    /// The condition a row read must make true, or `None` for every row.
    pub(crate) filter: Option<Pred>,
}

/// Plans `filter` on `table`: looks up its columns, checks its types, and
/// narrows the range to what the primary key conditions joined by
/// top-level ANDs allow.
pub(crate) fn plan(table: &Table, filter: Option<Expr>) -> Result<Plan, SqlError> {
    let mut range = (Bound::Unbounded, Bound::Unbounded);
    let Some(filter) = filter else {
        return Ok(Plan {
            range,
            filter: None,
        });
    };
    let filter = condition(table, filter)?;
    narrow(table, &filter, &mut range);
    Ok(Plan {
        range,
        filter: Some(filter),
    })
}

/// Checks the SET list of an UPDATE (design 0022, section 4-2): each
/// column exists and is named once, and each expression is a value of the
/// column's type, or a NULL literal outside the primary key. Returns each
/// column's position with its expression, in the order written. Like
/// [`plan`], it reads no row, so an UPDATE with a type error fails on an
/// empty table too.
pub(crate) fn assignments(
    table: &Table,
    assignments: Vec<(String, Expr)>,
) -> Result<Vec<(usize, Scalar)>, SqlError> {
    let mut checked: Vec<(usize, Scalar)> = Vec::with_capacity(assignments.len());
    for (name, expr) in assignments {
        let column = table
            .column(&name)
            .ok_or_else(|| table.no_such_column(&name))?;
        if checked.iter().any(|&(earlier, _)| earlier == column) {
            return Err(SqlError::DuplicateColumn(name));
        }
        let value = scalar(table, expr)?;
        let expected = table.columns[column].column_type;
        let fits = match value.column_type {
            None => column != table.primary_key,
            Some(found) => found == expected,
        };
        if !fits {
            return Err(SqlError::TypeMismatch {
                column: name,
                expected,
                found: value.column_type,
            });
        }
        checked.push((column, value.scalar));
    }
    Ok(checked)
}

// ------------------------------------------------------------ types

/// A checked value expression and its type: `None` for a NULL literal,
/// which fits any type.
struct Typed {
    scalar: Scalar,
    column_type: Option<ColumnType>,
}

/// `expr` as a condition, or a type error if it is a value.
fn condition(table: &Table, expr: Expr) -> Result<Pred, SqlError> {
    Ok(match expr {
        Expr::Not(inner) => Pred::Not(Box::new(condition(table, *inner)?)),
        Expr::And(left, right) => Pred::And(
            Box::new(condition(table, *left)?),
            Box::new(condition(table, *right)?),
        ),
        Expr::Or(left, right) => Pred::Or(
            Box::new(condition(table, *left)?),
            Box::new(condition(table, *right)?),
        ),
        leaf => return predicate(table, leaf),
    })
}

/// A condition that is not NOT, AND or OR: a comparison, IS NULL, BETWEEN
/// or IN, or a type error for a value.
#[inline(never)]
fn predicate(table: &Table, expr: Expr) -> Result<Pred, SqlError> {
    Ok(match expr {
        Expr::Compare { op, left, right } => {
            let left = scalar(table, *left)?;
            let right = scalar(table, *right)?;
            same_type(table, &[&left, &right])?;
            Pred::Compare {
                op,
                left: left.scalar,
                right: right.scalar,
            }
        }
        Expr::IsNull { operand, negated } => Pred::IsNull {
            operand: scalar(table, *operand)?.scalar,
            negated,
        },
        Expr::Between {
            operand,
            low,
            high,
            negated,
        } => {
            let operand = scalar(table, *operand)?;
            let low = scalar(table, *low)?;
            let high = scalar(table, *high)?;
            same_type(table, &[&operand, &low, &high])?;
            Pred::Between {
                operand: operand.scalar,
                low: low.scalar,
                high: high.scalar,
                negated,
            }
        }
        Expr::In {
            operand,
            list,
            negated,
        } => {
            let operand = scalar(table, *operand)?;
            let list = list
                .into_iter()
                .map(|item| scalar(table, item))
                .collect::<Result<Vec<_>, _>>()?;
            let mut all = vec![&operand];
            all.extend(&list);
            same_type(table, &all)?;
            Pred::In {
                operand: operand.scalar,
                list: list.into_iter().map(|item| item.scalar).collect(),
                negated,
            }
        }
        Expr::Not(_) | Expr::And(..) | Expr::Or(..) => condition(table, expr)?,
        value => {
            // Look its columns up first, so an unknown one says so.
            scalar(table, value)?;
            return Err(SqlError::Type(
                "a value where a true-or-false condition belongs; compare it with something".into(),
            ));
        }
    })
}

/// `expr` as a value with its type. A type error if it is a condition, or
/// if arithmetic meets a non-integer.
fn scalar(table: &Table, expr: Expr) -> Result<Typed, SqlError> {
    match expr {
        Expr::Neg(inner) => {
            let inner = scalar(table, *inner)?;
            integer_operand("-", inner.column_type)?;
            Ok(Typed {
                scalar: Scalar::Neg(Box::new(inner.scalar)),
                column_type: Some(ColumnType::Integer),
            })
        }
        Expr::Arith { op, left, right } => {
            let left = scalar(table, *left)?;
            let right = scalar(table, *right)?;
            integer_operand(op.symbol(), left.column_type)?;
            integer_operand(op.symbol(), right.column_type)?;
            Ok(Typed {
                scalar: Scalar::Arith {
                    op,
                    left: Box::new(left.scalar),
                    right: Box::new(right.scalar),
                },
                column_type: Some(ColumnType::Integer),
            })
        }
        leaf => leaf_scalar(table, leaf),
    }
}

/// A value that is not arithmetic: a literal or a column, or a type error
/// for a condition.
#[inline(never)]
fn leaf_scalar(table: &Table, expr: Expr) -> Result<Typed, SqlError> {
    match expr {
        Expr::Literal(v) => Ok(Typed {
            column_type: v.column_type(),
            scalar: Scalar::Literal(v),
        }),
        Expr::Column(name) => {
            let i = table
                .column(&name)
                .ok_or_else(|| table.no_such_column(&name))?;
            Ok(Typed {
                scalar: Scalar::Column(i),
                column_type: Some(table.columns[i].column_type),
            })
        }
        Expr::Neg(_) | Expr::Arith { .. } => scalar(table, expr),
        _ => Err(SqlError::Type(
            "a true-or-false condition where a value belongs".into(),
        )),
    }
}

/// Checks that an operand of arithmetic is an integer or NULL.
fn integer_operand(op: &str, found: Option<ColumnType>) -> Result<(), SqlError> {
    match found {
        None | Some(ColumnType::Integer) => Ok(()),
        Some(found) => Err(SqlError::Type(format!(
            "`{op}` works on INTEGER values, not on {found}"
        ))),
    }
}

/// Checks that the operands of a comparison, BETWEEN or IN have one type,
/// NULL literals aside. If a bare column takes part, the error names it as
/// [`SqlError::TypeMismatch`], as a value of the wrong type for a column
/// did before (design 0017); otherwise it is a [`SqlError::Type`].
fn same_type(table: &Table, operands: &[&Typed]) -> Result<(), SqlError> {
    let types = || operands.iter().filter_map(|operand| operand.column_type);
    let Some(first) = types().next() else {
        return Ok(());
    };
    if types().all(|t| t == first) {
        return Ok(());
    }
    let column = operands.iter().find_map(|operand| match operand.scalar {
        Scalar::Column(i) => Some((i, operand.column_type?)),
        _ => None,
    });
    if let Some((i, expected)) = column {
        let found = types().find(|&t| t != expected);
        return Err(SqlError::TypeMismatch {
            column: table.columns[i].name.clone(),
            expected,
            found,
        });
    }
    let other = types().find(|&t| t != first).unwrap_or(first);
    Err(SqlError::Type(format!(
        "cannot compare {first} with {other}"
    )))
}

// ------------------------------------------------------------ the range

/// Narrows `range` by the primary key conditions that `pred` holds at its
/// top level, joined by AND: each of them is needed for the row to match,
/// so every matching row lies in their intersection (design 0019,
/// section 4). Conditions under OR or NOT, and `!=`, NOT BETWEEN and NOT
/// IN, which would need more than one range, leave it as it is.
fn narrow(table: &Table, pred: &Pred, range: &mut KeyRange) {
    match pred {
        Pred::And(left, right) => {
            narrow(table, left, range);
            narrow(table, right, range);
        }
        leaf => narrow_by(table, leaf, range),
    }
}

/// Narrows `range` by one condition joined by AND at the top level.
#[inline(never)]
fn narrow_by(table: &Table, pred: &Pred, range: &mut KeyRange) {
    let is_key = |scalar: &Scalar| matches!(scalar, Scalar::Column(i) if *i == table.primary_key);
    match pred {
        Pred::Compare { op, left, right } => {
            let (op, constant) = if is_key(left) {
                (*op, right)
            } else if is_key(right) {
                (op.flip(), left)
            } else {
                return;
            };
            let Folded::Value(v) = fold(constant) else {
                return;
            };
            match op {
                CompareOp::Eq => {
                    raise_start(range, Bound::Included(v.clone()));
                    lower_end(range, Bound::Included(v));
                }
                CompareOp::Lt => lower_end(range, Bound::Excluded(v)),
                CompareOp::Le => lower_end(range, Bound::Included(v)),
                CompareOp::Gt => raise_start(range, Bound::Excluded(v)),
                CompareOp::Ge => raise_start(range, Bound::Included(v)),
                CompareOp::Ne => {}
            }
        }
        Pred::Between {
            operand,
            low,
            high,
            negated: false,
        } if is_key(operand) => {
            // Each end alone is needed for the row to match.
            if let Folded::Value(low) = fold(low) {
                raise_start(range, Bound::Included(low));
            }
            if let Folded::Value(high) = fold(high) {
                lower_end(range, Bound::Included(high));
            }
        }
        Pred::In {
            operand,
            list,
            negated: false,
        } if is_key(operand) => {
            // From the smallest value to the largest: generous between
            // them, exact once the filter checks each row. A NULL item
            // matches nothing, so it does not widen the range.
            let mut values = Vec::with_capacity(list.len());
            for item in list {
                match fold(item) {
                    Folded::Value(v) => values.push((key(&v), v)),
                    Folded::Null => {}
                    Folded::Not => return,
                }
            }
            let by_key = |a: &&(Vec<u8>, Value), b: &&(Vec<u8>, Value)| a.0.cmp(&b.0);
            let (Some(min), Some(max)) =
                (values.iter().min_by(by_key), values.iter().max_by(by_key))
            else {
                return;
            };
            let (min, max) = (min.1.clone(), max.1.clone());
            raise_start(range, Bound::Included(min));
            lower_end(range, Bound::Included(max));
        }
        _ => {}
    }
}

/// What a value expression folds to while planning.
enum Folded {
    /// It reads no column and computes to this value, never NULL.
    Value(Value),
    /// It reads no column and computes to NULL, which no key equals.
    Null,
    /// It reads a column, or computing it fails. A constant that
    /// overflows is not folded: planning would then fail even on an empty
    /// table, where the statement reads no row and so computes nothing
    /// (design 0019, section 4).
    Not,
}

/// Folds `scalar`, which the range may use only as [`Folded::Value`]. So
/// NULL never reaches a range, where [`encode_key`] could not encode it.
fn fold(scalar: &Scalar) -> Folded {
    if !scalar.is_constant() {
        return Folded::Not;
    }
    match scalar.eval(&[]) {
        Ok(v) if *v == Value::Null => Folded::Null,
        Ok(v) => Folded::Value(v.into_owned()),
        Err(_) => Folded::Not,
    }
}

/// The encoded key of a primary key value, which sorts as the value does
/// (design 0017, C4), so one byte comparison serves every type.
fn key(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    encode_key(v, &mut out);
    out
}

/// Moves the start of `range` up to `start` if that is later. At the same
/// value, an excluded start is the later one.
fn raise_start(range: &mut KeyRange, start: Bound<Value>) {
    if tighter(&start, &range.0, Ordering::Greater) {
        range.0 = start;
    }
}

/// Moves the end of `range` down to `end` if that is earlier. At the same
/// value, an excluded end is the earlier one.
fn lower_end(range: &mut KeyRange, end: Bound<Value>) {
    if tighter(&end, &range.1, Ordering::Less) {
        range.1 = end;
    }
}

/// Whether bound `new` leaves fewer keys than `old` on its side: its value
/// lies further in direction `inward`, or the same value is excluded.
fn tighter(new: &Bound<Value>, old: &Bound<Value>, inward: Ordering) -> bool {
    let (new_value, new_excluded) = match new {
        Bound::Unbounded => return false,
        Bound::Included(v) => (v, false),
        Bound::Excluded(v) => (v, true),
    };
    let (old_value, old_excluded) = match old {
        Bound::Unbounded => return true,
        Bound::Included(v) => (v, false),
        Bound::Excluded(v) => (v, true),
    };
    let ordering = key(new_value).cmp(&key(old_value));
    ordering == inward || (ordering.is_eq() && new_excluded && !old_excluded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Column;
    use crate::parser::{parse, Statement};

    /// `t (id INTEGER PRIMARY KEY, age INTEGER, name TEXT)`.
    fn table() -> Table {
        let column = |name: &str, column_type| Column {
            name: name.into(),
            column_type,
        };
        Table {
            name: "t".into(),
            id: 1,
            columns: vec![
                column("id", ColumnType::Integer),
                column("age", ColumnType::Integer),
                column("name", ColumnType::Text),
            ],
            primary_key: 0,
        }
    }

    /// `s (name TEXT, k TEXT PRIMARY KEY)`.
    fn text_keyed() -> Table {
        Table {
            name: "s".into(),
            id: 2,
            columns: vec![
                Column {
                    name: "name".into(),
                    column_type: ColumnType::Text,
                },
                Column {
                    name: "k".into(),
                    column_type: ColumnType::Text,
                },
            ],
            primary_key: 1,
        }
    }

    fn plan_of(table: &Table, condition: &str) -> Plan {
        let sql = format!("SELECT * FROM {} WHERE {condition}", table.name);
        match parse(&sql).unwrap() {
            Statement::Select { filter, .. } => plan(table, filter).unwrap(),
            other => panic!("{other:?}"),
        }
    }

    fn range(condition: &str) -> KeyRange {
        plan_of(&table(), condition).range
    }

    use Bound::{Excluded as Ex, Included as In, Unbounded as Un};

    fn i(v: i64) -> Value {
        Value::Integer(v)
    }

    #[test]
    fn primary_key_comparisons_become_ranges() {
        for (condition, want) in [
            ("id = 4", (In(i(4)), In(i(4)))),
            ("id > 5", (Ex(i(5)), Un)),
            ("id >= 5", (In(i(5)), Un)),
            ("id < 5", (Un, Ex(i(5)))),
            ("id <= 5", (Un, In(i(5)))),
            ("5 < id", (Ex(i(5)), Un)),
            ("5 >= id", (Un, In(i(5)))),
            ("id > -9223372036854775808", (Ex(i(i64::MIN)), Un)),
        ] {
            assert_eq!(range(condition), want, "{condition}");
        }
    }

    #[test]
    fn top_level_and_intersects_and_other_columns_only_filter() {
        for (condition, want) in [
            ("id > 5 AND name = 'a'", (Ex(i(5)), Un)),
            (
                "id >= 2 AND id < 9 AND id > 2 AND id <= 9",
                (Ex(i(2)), Ex(i(9))),
            ),
            ("id <= 9 AND id < 9", (Un, Ex(i(9)))),
            ("id >= 3 AND id > 3", (Ex(i(3)), Un)),
            ("(id > 1 AND (age = 2 AND id < 9))", (Ex(i(1)), Ex(i(9)))),
            // Backwards: the engine scans nothing.
            ("id > 5 AND id < 3", (Ex(i(5)), Ex(i(3)))),
        ] {
            assert_eq!(range(condition), want, "{condition}");
        }
    }

    #[test]
    fn between_in_and_constant_arithmetic_become_ranges() {
        for (condition, want) in [
            ("id BETWEEN 2 AND 4", (In(i(2)), In(i(4)))),
            ("id BETWEEN 2 AND NULL", (In(i(2)), Un)),
            ("id IN (7, 3, NULL, 5)", (In(i(3)), In(i(7)))),
            ("id IN (-1)", (In(i(-1)), In(i(-1)))),
            ("id > 3 + 4", (Ex(i(7)), Un)),
            ("id < -(2 * 3)", (Un, Ex(i(-6)))),
            ("id IN (1 + 1, 10 % 4)", (In(i(2)), In(i(2)))),
        ] {
            assert_eq!(range(condition), want, "{condition}");
        }
    }

    #[test]
    fn what_needs_more_than_one_range_or_a_row_scans_the_table() {
        for condition in [
            "id = 1 OR id = 5",
            "NOT (id < 5)",
            "id != 3",
            "id <> 3",
            "id NOT BETWEEN 1 AND 2",
            "id NOT IN (1)",
            "age > 5",
            "id + 1 > 5",
            "id = age",
            "id IN (1, age)",
            // NULL never reaches a range, where `encode_key` could not
            // encode it (review S6).
            "id = NULL",
            "id > 1 + NULL",
            "id IN (NULL)",
            "id IN (NULL, NULL / 0)",
            "id BETWEEN NULL AND NULL",
            "NULL < id",
            "id IS NOT NULL",
            // A constant that overflows is left to the filter.
            "id > 9223372036854775807 + 1",
            "id IN (1, 1 / 0)",
        ] {
            assert_eq!(range(condition), (Un, Un), "{condition}");
        }
    }

    #[test]
    fn the_filter_is_always_the_whole_condition() {
        let plan = plan_of(&table(), "id > 5 AND name = 'a'");
        assert!(matches!(plan.filter, Some(Pred::And(..))));
    }

    #[test]
    fn no_where_scans_the_whole_table_without_a_filter() {
        assert_eq!(
            plan(&table(), None).unwrap(),
            Plan {
                range: (Un, Un),
                filter: None
            }
        );
    }

    #[test]
    fn text_keys_range_by_their_bytes() {
        let t = |s: &str| Value::Text(s.into());
        let table = text_keyed();
        assert_eq!(
            plan_of(&table, "k > 'a' AND k <= 'b' AND name = 'x'").range,
            (Ex(t("a")), In(t("b")))
        );
        assert_eq!(
            plan_of(&table, "k IN ('b', 'ab', 'a')").range,
            (In(t("a")), In(t("b")))
        );
        // Only the primary key narrows the range.
        assert_eq!(plan_of(&table, "name = 'x'").range, (Un, Un));
    }

    fn assignments_of(set: &str) -> Result<Vec<(usize, Scalar)>, SqlError> {
        match parse(&format!("UPDATE t SET {set}")).unwrap() {
            Statement::Update { assignments: a, .. } => assignments(&table(), a),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn set_gives_column_positions_in_the_order_written() {
        assert_eq!(
            assignments_of("name = 'x', id = age").unwrap(),
            vec![
                (2, Scalar::Literal(Value::Text("x".into()))),
                (0, Scalar::Column(1)),
            ]
        );
        // NULL fits any column but the primary key.
        assert_eq!(
            assignments_of("age = NULL").unwrap(),
            vec![(1, Scalar::Literal(Value::Null))]
        );
    }

    #[test]
    fn set_checks_columns_and_types_without_a_row() {
        assert!(matches!(
            assignments_of("nope = 1"),
            Err(SqlError::NoSuchColumn { column, .. }) if column == "nope"
        ));
        assert!(matches!(
            assignments_of("age = 1, age = 2"),
            Err(SqlError::DuplicateColumn(c)) if c == "age"
        ));
        assert!(matches!(
            assignments_of("age = name"),
            Err(SqlError::TypeMismatch { column, expected: ColumnType::Integer, found: Some(ColumnType::Text) }) if column == "age"
        ));
        assert!(matches!(
            assignments_of("id = NULL"),
            Err(SqlError::TypeMismatch { column, found: None, .. }) if column == "id"
        ));
        assert!(matches!(
            assignments_of("age = (age = 1)"),
            Err(SqlError::Type(_))
        ));
        assert!(matches!(
            assignments_of("age = name + 1"),
            Err(SqlError::Type(_))
        ));
    }
}
