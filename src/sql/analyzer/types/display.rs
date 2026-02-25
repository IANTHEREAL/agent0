//! Display trait implementations for the Typed IR types.
//!
//! Provides human-readable formatting for `TypedExpr`, `BinaryOp`, `UnaryOp`,
//! and `IsTestKind` — used by EXPLAIN output, error messages, and debugging.

use std::fmt;

use super::*;

impl fmt::Display for TypedExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            TypedExprKind::Constant(v) => write!(f, "{}", v),
            TypedExprKind::ColumnRef { column_name, .. } => write!(f, "{}", column_name),
            TypedExprKind::BinaryOp { left, op, right } => {
                write!(f, "({} {} {})", left, op, right)
            }
            TypedExprKind::UnaryOp { op, operand } => write!(f, "({}{})", op, operand),
            TypedExprKind::Cast {
                expr, target_type, ..
            } => {
                write!(f, "CAST({} AS {:?})", expr, target_type)
            }
            TypedExprKind::IsTest {
                expr,
                test,
                negated,
            } => {
                write!(
                    f,
                    "({} IS {}{})",
                    expr,
                    if *negated { "NOT " } else { "" },
                    test
                )
            }
            TypedExprKind::IsDistinctFrom {
                left,
                right,
                negated,
            } => {
                write!(
                    f,
                    "({} IS {}DISTINCT FROM {})",
                    left,
                    if *negated { "NOT " } else { "" },
                    right
                )
            }
            TypedExprKind::Between {
                expr,
                low,
                high,
                negated,
            } => {
                write!(
                    f,
                    "({} {}BETWEEN {} AND {})",
                    expr,
                    if *negated { "NOT " } else { "" },
                    low,
                    high
                )
            }
            TypedExprKind::InList {
                expr,
                list,
                negated,
            } => {
                let items: Vec<String> = list.iter().map(|e| format!("{}", e)).collect();
                write!(
                    f,
                    "({} {}IN ({}))",
                    expr,
                    if *negated { "NOT " } else { "" },
                    items.join(", ")
                )
            }
            TypedExprKind::Like {
                expr,
                pattern,
                negated,
                case_insensitive,
                ..
            } => {
                let kw = if *case_insensitive { "ILIKE" } else { "LIKE" };
                write!(
                    f,
                    "({} {}{}  {})",
                    expr,
                    if *negated { "NOT " } else { "" },
                    kw,
                    pattern
                )
            }
            TypedExprKind::SimilarTo {
                expr,
                pattern,
                negated,
                ..
            } => {
                write!(
                    f,
                    "({} {}SIMILAR TO {})",
                    expr,
                    if *negated { "NOT " } else { "" },
                    pattern
                )
            }
            TypedExprKind::Case {
                operand,
                when_clauses,
                else_result,
            } => {
                write!(f, "CASE")?;
                if let Some(op) = operand {
                    write!(f, " {}", op)?;
                }
                for (when, then) in when_clauses {
                    write!(f, " WHEN {} THEN {}", when, then)?;
                }
                if let Some(el) = else_result {
                    write!(f, " ELSE {}", el)?;
                }
                write!(f, " END")
            }
            TypedExprKind::Coalesce(args) => {
                let items: Vec<String> = args.iter().map(|e| format!("{}", e)).collect();
                write!(f, "COALESCE({})", items.join(", "))
            }
            TypedExprKind::NullIf(a, b) => write!(f, "NULLIF({}, {})", a, b),
            TypedExprKind::MinMax { args, is_greatest } => {
                let items: Vec<String> = args.iter().map(|e| format!("{}", e)).collect();
                let name = if *is_greatest { "GREATEST" } else { "LEAST" };
                write!(f, "{}({})", name, items.join(", "))
            }
            TypedExprKind::FunctionCall { func, args, .. } => {
                let items: Vec<String> = args.iter().map(|e| format!("{}", e)).collect();
                write!(f, "{}({})", func.name, items.join(", "))
            }
            TypedExprKind::AggregateCall {
                func,
                args,
                distinct,
                ..
            } => {
                let items: Vec<String> = args.iter().map(|e| format!("{}", e)).collect();
                write!(
                    f,
                    "{}({}{})",
                    func.name,
                    if *distinct { "DISTINCT " } else { "" },
                    if items.is_empty() {
                        "*".to_string()
                    } else {
                        items.join(", ")
                    }
                )
            }
            TypedExprKind::WindowCall { func, args, .. } => {
                let items: Vec<String> = args.iter().map(|e| format!("{}", e)).collect();
                write!(
                    f,
                    "{}({}) OVER (..)",
                    func.name,
                    if items.is_empty() {
                        "*".to_string()
                    } else {
                        items.join(", ")
                    }
                )
            }
            TypedExprKind::ScalarSubquery(_) => write!(f, "(subquery)"),
            TypedExprKind::ArraySubquery(_) => write!(f, "ARRAY(subquery)"),
            TypedExprKind::Exists { negated, .. } => {
                write!(f, "{}EXISTS (subquery)", if *negated { "NOT " } else { "" })
            }
            TypedExprKind::InSubquery { expr, negated, .. } => {
                write!(
                    f,
                    "({} {}IN (subquery))",
                    expr,
                    if *negated { "NOT " } else { "" }
                )
            }
            TypedExprKind::TupleInSubquery { exprs, negated, .. } => {
                let items: Vec<String> = exprs.iter().map(|e| format!("{}", e)).collect();
                write!(
                    f,
                    "(({}) {}IN (subquery))",
                    items.join(", "),
                    if *negated { "NOT " } else { "" }
                )
            }
            TypedExprKind::AnyAll {
                expr, op, is_all, ..
            } => {
                let kw = if *is_all { "ALL" } else { "ANY" };
                write!(f, "({} {} {} (subquery))", expr, op, kw)
            }
            TypedExprKind::ArrayLiteral(elems) => {
                let items: Vec<String> = elems.iter().map(|e| format!("{}", e)).collect();
                write!(f, "ARRAY[{}]", items.join(", "))
            }
            TypedExprKind::ArrayIndex { array, index } => {
                write!(f, "{}[{}]", array, index)
            }
            TypedExprKind::JsonAccess {
                expr,
                path,
                operator,
            } => {
                let op_str = match operator {
                    JsonAccessOp::Arrow => "->",
                    JsonAccessOp::LongArrow => "->>",
                    JsonAccessOp::HashArrow => "#>",
                    JsonAccessOp::HashLongArrow => "#>>",
                    JsonAccessOp::HashMinus => "#-",
                };
                write!(f, "({} {} {})", expr, op_str, path)
            }
            TypedExprKind::Row(elems) => {
                let items: Vec<String> = elems.iter().map(|e| format!("{}", e)).collect();
                write!(f, "ROW({})", items.join(", "))
            }
            TypedExprKind::Collate {
                expr, collation, ..
            } => {
                write!(f, "({} COLLATE {})", expr, collation)
            }
            TypedExprKind::Default => write!(f, "DEFAULT"),
            TypedExprKind::Parameter { index } => write!(f, "${}", index + 1),
        }
    }
}

impl fmt::Display for BinaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Add => write!(f, "+"),
            Self::Sub => write!(f, "-"),
            Self::Mul => write!(f, "*"),
            Self::Div => write!(f, "/"),
            Self::Mod => write!(f, "%"),
            Self::Exp => write!(f, "^"),
            Self::Eq => write!(f, "="),
            Self::NotEq => write!(f, "<>"),
            Self::Lt => write!(f, "<"),
            Self::LtEq => write!(f, "<="),
            Self::Gt => write!(f, ">"),
            Self::GtEq => write!(f, ">="),
            Self::And => write!(f, "AND"),
            Self::Or => write!(f, "OR"),
            Self::Concat => write!(f, "||"),
            Self::BitwiseAnd => write!(f, "&"),
            Self::BitwiseOr => write!(f, "|"),
            Self::BitwiseXor => write!(f, "#"),
            Self::ShiftLeft => write!(f, "<<"),
            Self::ShiftRight => write!(f, ">>"),
            Self::RegexMatch => write!(f, "~"),
            Self::RegexIMatch => write!(f, "~*"),
            Self::RegexNotMatch => write!(f, "!~"),
            Self::RegexNotIMatch => write!(f, "!~*"),
            Self::ArrayOverlap => write!(f, "&&"),
            Self::ArrayContains | Self::JsonContains => write!(f, "@>"),
            Self::ArrayContainedBy | Self::JsonContainedBy => write!(f, "<@"),
            Self::JsonExists => write!(f, "?"),
            Self::JsonExistsAny => write!(f, "?|"),
            Self::JsonExistsAll => write!(f, "?&"),
            Self::TsMatch => write!(f, "@@"),
            Self::Custom(s) => write!(f, "{}", s),
        }
    }
}

impl fmt::Display for UnaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Not => write!(f, "NOT"),
            Self::Minus => write!(f, "-"),
            Self::Plus => write!(f, "+"),
            Self::BitwiseNot => write!(f, "~"),
        }
    }
}

impl fmt::Display for IsTestKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => write!(f, "NULL"),
            Self::True => write!(f, "TRUE"),
            Self::False => write!(f, "FALSE"),
            Self::Unknown => write!(f, "UNKNOWN"),
        }
    }
}
