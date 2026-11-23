//! Expressions in WHERE (issue #32, design 0019): the tree the parser
//! builds, the checked trees the planner turns it into, and how a checked
//! tree is computed on a row.
//!
//! The parser cannot tell a value from a condition inside parentheses, so
//! it builds one [`Expr`] for both, with column names. The planner checks
//! the types and splits it into [`Scalar`] (a value) and [`Pred`] (a
//! condition), with column positions: a condition used as a value, or a
//! value used as a condition, cannot be built as either, so computing one
//! never meets such a case.

use std::borrow::Cow;
use std::cmp::Ordering;

use crate::{SqlError, Value};

/// An expression as the parser reads it: names not yet looked up, types
/// not yet checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Expr {
    Literal(Value),
    Column(String),
    /// Unary minus.
    Neg(Box<Expr>),
    Arith {
        op: ArithOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Compare {
        op: CompareOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    IsNull {
        operand: Box<Expr>,
        negated: bool,
    },
    Between {
        operand: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
    },
    In {
        operand: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
}

/// An integer operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

/// A comparison operator. `!=` and `<>` are both [`CompareOp::Ne`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl ArithOp {
    pub(crate) fn symbol(self) -> &'static str {
        match self {
            ArithOp::Add => "+",
            ArithOp::Sub => "-",
            ArithOp::Mul => "*",
            ArithOp::Div => "/",
            ArithOp::Rem => "%",
        }
    }

    /// `l op r`, or an error if it overflows or divides by zero (design
    /// 0019, section 2-2). Division truncates toward zero and the
    /// remainder takes the sign of `l`, as Rust's do.
    fn apply(self, l: i64, r: i64) -> Result<i64, SqlError> {
        if matches!(self, ArithOp::Div | ArithOp::Rem) && r == 0 {
            return Err(SqlError::Arithmetic(format!(
                "division by zero: {l} {} 0",
                self.symbol()
            )));
        }
        let result = match self {
            ArithOp::Add => l.checked_add(r),
            ArithOp::Sub => l.checked_sub(r),
            ArithOp::Mul => l.checked_mul(r),
            ArithOp::Div => l.checked_div(r),
            ArithOp::Rem => l.checked_rem(r),
        };
        result.ok_or_else(|| {
            SqlError::Arithmetic(format!(
                "integer overflow: {l} {} {r} does not fit in 64 bits",
                self.symbol()
            ))
        })
    }
}

impl CompareOp {
    /// The operator that says the same with its sides swapped: `5 < id`
    /// is `id > 5`.
    pub(crate) fn flip(self) -> Self {
        match self {
            CompareOp::Lt => CompareOp::Gt,
            CompareOp::Le => CompareOp::Ge,
            CompareOp::Gt => CompareOp::Lt,
            CompareOp::Ge => CompareOp::Le,
            same => same,
        }
    }

    fn holds(self, ordering: Ordering) -> bool {
        match self {
            CompareOp::Eq => ordering.is_eq(),
            CompareOp::Ne => ordering.is_ne(),
            CompareOp::Lt => ordering.is_lt(),
            CompareOp::Le => ordering.is_le(),
            CompareOp::Gt => ordering.is_gt(),
            CompareOp::Ge => ordering.is_ge(),
        }
    }
}

/// A value expression whose types the planner checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Scalar {
    Literal(Value),
    /// The column at this position in the row.
    Column(usize),
    Neg(Box<Scalar>),
    Arith {
        op: ArithOp,
        left: Box<Scalar>,
        right: Box<Scalar>,
    },
}

/// A condition whose types the planner checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Pred {
    Compare {
        op: CompareOp,
        left: Scalar,
        right: Scalar,
    },
    IsNull {
        operand: Scalar,
        negated: bool,
    },
    Between {
        operand: Scalar,
        low: Scalar,
        high: Scalar,
        negated: bool,
    },
    In {
        operand: Scalar,
        list: Vec<Scalar>,
        negated: bool,
    },
    Not(Box<Pred>),
    And(Box<Pred>, Box<Pred>),
    Or(Box<Pred>, Box<Pred>),
}

/// What a condition is on a row: SQL's three-valued logic (design 0019,
/// section 2). WHERE keeps a row only if its condition is `True`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Truth {
    True,
    False,
    /// A NULL took part, so the answer is not known.
    Unknown,
}

impl Truth {
    fn from_bool(b: bool) -> Self {
        if b {
            Truth::True
        } else {
            Truth::False
        }
    }

    fn not(self) -> Self {
        match self {
            Truth::True => Truth::False,
            Truth::False => Truth::True,
            Truth::Unknown => Truth::Unknown,
        }
    }

    /// `NOT self` if `negated`, else `self`.
    fn negate_if(self, negated: bool) -> Self {
        if negated {
            self.not()
        } else {
            self
        }
    }

    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Truth::False, _) | (_, Truth::False) => Truth::False,
            (Truth::True, Truth::True) => Truth::True,
            _ => Truth::Unknown,
        }
    }

    fn or(self, other: Self) -> Self {
        match (self, other) {
            (Truth::True, _) | (_, Truth::True) => Truth::True,
            (Truth::False, Truth::False) => Truth::False,
            _ => Truth::Unknown,
        }
    }
}

impl Scalar {
    /// Whether the expression reads no column, so its value is the same
    /// on every row and can be computed once, without one.
    pub(crate) fn is_constant(&self) -> bool {
        match self {
            Scalar::Literal(_) => true,
            Scalar::Column(_) => false,
            Scalar::Neg(e) => e.is_constant(),
            Scalar::Arith { left, right, .. } => left.is_constant() && right.is_constant(),
        }
    }

    /// The value on `row`, which holds a value for every column the
    /// expression reads. Borrows a column's or literal's value rather
    /// than copying it.
    pub(crate) fn eval<'a>(&'a self, row: &'a [Value]) -> Result<Cow<'a, Value>, SqlError> {
        Ok(match self {
            Scalar::Literal(value) => Cow::Borrowed(value),
            Scalar::Column(i) => Cow::Borrowed(&row[*i]),
            Scalar::Neg(e) => Cow::Owned(match e.eval(row)?.as_ref() {
                Value::Null => Value::Null,
                Value::Integer(i) => Value::Integer(i.checked_neg().ok_or_else(|| {
                    SqlError::Arithmetic(format!(
                        "integer overflow: -({i}) does not fit in 64 bits"
                    ))
                })?),
                other => return Err(not_integer(other)),
            }),
            Scalar::Arith { op, left, right } => {
                let left = left.eval(row)?;
                let right = right.eval(row)?;
                Cow::Owned(match (left.as_ref(), right.as_ref()) {
                    (Value::Null, _) | (_, Value::Null) => Value::Null,
                    (Value::Integer(l), Value::Integer(r)) => Value::Integer(op.apply(*l, *r)?),
                    (Value::Integer(_), other) | (other, _) => return Err(not_integer(other)),
                })
            }
        })
    }
}

impl Pred {
    /// The condition on `row`. AND stops at a false left side, OR at a
    /// true one, BETWEEN at a false lower bound and IN at the first
    /// match: the answer is the same, but what is not computed cannot
    /// overflow (design 0019, section 2-1).
    pub(crate) fn eval(&self, row: &[Value]) -> Result<Truth, SqlError> {
        Ok(match self {
            Pred::Compare { op, left, right } => {
                compare(*op, &*left.eval(row)?, &*right.eval(row)?)?
            }
            Pred::IsNull { operand, negated } => {
                Truth::from_bool(matches!(*operand.eval(row)?, Value::Null)).negate_if(*negated)
            }
            Pred::Between {
                operand,
                low,
                high,
                negated,
            } => {
                let value = operand.eval(row)?;
                let above = compare(CompareOp::Ge, &value, &*low.eval(row)?)?;
                let within = if above == Truth::False {
                    Truth::False
                } else {
                    above.and(compare(CompareOp::Le, &value, &*high.eval(row)?)?)
                };
                within.negate_if(*negated)
            }
            Pred::In {
                operand,
                list,
                negated,
            } => {
                let value = operand.eval(row)?;
                let mut found = if matches!(*value, Value::Null) {
                    Truth::Unknown
                } else {
                    Truth::False
                };
                if found == Truth::False {
                    for item in list {
                        match compare(CompareOp::Eq, &value, &*item.eval(row)?)? {
                            Truth::True => {
                                found = Truth::True;
                                break;
                            }
                            Truth::Unknown => found = Truth::Unknown,
                            Truth::False => {}
                        }
                    }
                }
                found.negate_if(*negated)
            }
            Pred::Not(e) => e.eval(row)?.not(),
            Pred::And(left, right) => match left.eval(row)? {
                Truth::False => Truth::False,
                left => left.and(right.eval(row)?),
            },
            Pred::Or(left, right) => match left.eval(row)? {
                Truth::True => Truth::True,
                left => left.or(right.eval(row)?),
            },
        })
    }
}

/// `l op r`: unknown if either is NULL. Integers compare by size, text and
/// blobs bytewise, the order their keys sort in (design 0019, section 3).
fn compare(op: CompareOp, l: &Value, r: &Value) -> Result<Truth, SqlError> {
    let ordering = match (l, r) {
        (Value::Null, _) | (_, Value::Null) => return Ok(Truth::Unknown),
        (Value::Integer(l), Value::Integer(r)) => l.cmp(r),
        (Value::Text(l), Value::Text(r)) => l.cmp(r),
        (Value::Blob(l), Value::Blob(r)) => l.cmp(r),
        // The planner checked that both sides have one type, so only a
        // stored value of another type than its column gets here.
        (l, r) => {
            return Err(SqlError::Corrupt(format!(
                "a row compares {:?} with {:?}: a column holds a value of another type",
                l.column_type(),
                r.column_type()
            )))
        }
    };
    Ok(Truth::from_bool(op.holds(ordering)))
}

/// The error for a non-integer where the planner checked for an integer:
/// only a stored value of another type than its column gets here.
fn not_integer(value: &Value) -> SqlError {
    SqlError::Corrupt(format!(
        "arithmetic on a {:?} value: a column holds a value of another type",
        value.column_type()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(i: i64) -> Scalar {
        Scalar::Literal(Value::Integer(i))
    }

    fn null() -> Scalar {
        Scalar::Literal(Value::Null)
    }

    fn arith(op: ArithOp, l: Scalar, r: Scalar) -> Scalar {
        Scalar::Arith {
            op,
            left: Box::new(l),
            right: Box::new(r),
        }
    }

    fn value(e: &Scalar) -> Result<Value, SqlError> {
        e.eval(&[]).map(Cow::into_owned)
    }

    fn truth(p: &Pred) -> Truth {
        p.eval(&[]).unwrap()
    }

    fn eq(l: Scalar, r: Scalar) -> Pred {
        Pred::Compare {
            op: CompareOp::Eq,
            left: l,
            right: r,
        }
    }

    #[test]
    fn three_valued_and_or_not_follow_the_tables() {
        use Truth::*;
        let all = [True, False, Unknown];
        for a in all {
            for b in all {
                let and = match (a, b) {
                    (False, _) | (_, False) => False,
                    (True, True) => True,
                    _ => Unknown,
                };
                let or = match (a, b) {
                    (True, _) | (_, True) => True,
                    (False, False) => False,
                    _ => Unknown,
                };
                assert_eq!(a.and(b), and, "{a:?} AND {b:?}");
                assert_eq!(a.or(b), or, "{a:?} OR {b:?}");
            }
        }
        assert_eq!(Unknown.not(), Unknown);
    }

    #[test]
    fn integer_arithmetic_truncates_and_checks() {
        use ArithOp::*;
        assert_eq!(
            value(&arith(Div, int(-7), int(2))).unwrap(),
            Value::Integer(-3)
        );
        assert_eq!(
            value(&arith(Rem, int(-7), int(2))).unwrap(),
            Value::Integer(-1)
        );
        assert_eq!(
            value(&arith(Rem, int(7), int(-2))).unwrap(),
            Value::Integer(1)
        );
        for (op, l, r) in [
            (Add, i64::MAX, 1),
            (Sub, i64::MIN, 1),
            (Mul, i64::MAX, 2),
            (Div, i64::MIN, -1),
            (Rem, i64::MIN, -1),
            (Div, 1, 0),
            (Rem, 1, 0),
        ] {
            assert!(
                matches!(
                    value(&arith(op, int(l), int(r))),
                    Err(SqlError::Arithmetic(_))
                ),
                "{l} {} {r}",
                op.symbol()
            );
        }
        assert!(matches!(
            value(&Scalar::Neg(Box::new(int(i64::MIN)))),
            Err(SqlError::Arithmetic(_))
        ));
    }

    #[test]
    fn null_in_arithmetic_is_null_even_when_dividing_by_zero() {
        assert_eq!(
            value(&arith(ArithOp::Div, null(), int(0))).unwrap(),
            Value::Null
        );
        assert_eq!(
            value(&arith(ArithOp::Add, int(1), null())).unwrap(),
            Value::Null
        );
        assert_eq!(value(&Scalar::Neg(Box::new(null()))).unwrap(), Value::Null);
    }

    #[test]
    fn in_and_between_with_null_are_unknown_unless_decided() {
        let list = |items: Vec<Scalar>, negated| Pred::In {
            operand: int(3),
            list: items,
            negated,
        };
        assert_eq!(truth(&list(vec![int(1), null()], false)), Truth::Unknown);
        assert_eq!(truth(&list(vec![int(1), null()], true)), Truth::Unknown);
        assert_eq!(truth(&list(vec![int(3), null()], true)), Truth::False);
        assert_eq!(truth(&list(vec![int(1), int(2)], true)), Truth::True);
        let between = |low, high| Pred::Between {
            operand: int(0),
            low,
            high,
            negated: true,
        };
        // 0 >= 1 is false, so the whole BETWEEN is false and NOT is true.
        assert_eq!(truth(&between(int(1), null())), Truth::True);
        assert_eq!(truth(&between(int(-1), null())), Truth::Unknown);
    }

    #[test]
    fn shortcuts_skip_what_cannot_change_the_answer() {
        let boom = || eq(arith(ArithOp::Div, int(1), int(0)), int(1));
        let falsy = || eq(int(1), int(2));
        let truthy = || eq(int(1), int(1));
        assert_eq!(
            truth(&Pred::And(Box::new(falsy()), Box::new(boom()))),
            Truth::False
        );
        assert_eq!(
            truth(&Pred::Or(Box::new(truthy()), Box::new(boom()))),
            Truth::True
        );
        assert!(Pred::And(Box::new(truthy()), Box::new(boom()))
            .eval(&[])
            .is_err());
    }

    #[test]
    fn a_stored_value_of_another_type_is_corrupt_not_a_panic() {
        let row = [Value::Text("x".into())];
        assert!(matches!(
            eq(Scalar::Column(0), int(1)).eval(&row),
            Err(SqlError::Corrupt(_))
        ));
        assert!(matches!(
            arith(ArithOp::Add, Scalar::Column(0), int(1)).eval(&row),
            Err(SqlError::Corrupt(_))
        ));
    }
}
