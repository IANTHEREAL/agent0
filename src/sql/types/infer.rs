//! Core type inference logic

use std::collections::HashMap;

use sqlparser::ast::{
    DataType as SqlDataType, Expr, Function, FunctionArg, FunctionArgExpr, Query, SelectItem,
    SetExpr,
};

use crate::types::DataType;

use super::coercion::{binary_op_result_type, unify_types};
use super::context::TypeContext;
use super::error::TypeError;
use super::registry::global_registry;

pub struct TypeInferrer<'a> {
    ctx: TypeContext<'a>,
    #[allow(dead_code)]
    cache: Option<HashMap<usize, DataType>>,
}

impl<'a> TypeInferrer<'a> {
    pub fn new(ctx: TypeContext<'a>) -> Self {
        Self { ctx, cache: None }
    }

    #[allow(dead_code)]
    pub fn with_cache(mut self) -> Self {
        self.cache = Some(HashMap::new());
        self
    }

    pub fn infer(&mut self, expr: &Expr) -> Result<DataType, TypeError> {
        self.infer_uncached(expr)
    }

    fn infer_uncached(&mut self, expr: &Expr) -> Result<DataType, TypeError> {
        match expr {
            Expr::Identifier(ident) => {
                let col = self.ctx.resolve_column(&ident.value)?;
                Ok(col.column_def.data_type.clone())
            }

            Expr::CompoundIdentifier(parts) => {
                if parts.is_empty() {
                    return Ok(DataType::Text);
                }

                // First try: full name as a single column (for JOIN schemas with "table.col" column names)
                let full_name = parts
                    .iter()
                    .map(|p| p.value.as_str())
                    .collect::<Vec<_>>()
                    .join(".");
                if let Ok(col) = self.ctx.resolve_column(&full_name) {
                    return Ok(col.column_def.data_type.clone());
                }

                // For schema-qualified identifiers ("schema.table.col"), JOIN type inference often
                // stores columns as "table.col". Try resolving using the last two parts before
                // falling back to unqualified lookup.
                if parts.len() > 2 {
                    let n = parts.len();
                    let last_two = format!("{}.{}", parts[n - 2].value, parts[n - 1].value);
                    if let Ok(col) = self.ctx.resolve_column(&last_two) {
                        return Ok(col.column_def.data_type.clone());
                    }
                }

                // Second try: qualified lookup
                if parts.len() == 2 {
                    if let Ok(col) = self.ctx.resolve_qualified(&parts[0].value, &parts[1].value) {
                        return Ok(col.column_def.data_type.clone());
                    }
                } else if parts.len() > 2 {
                    // schema.table.column - use last two parts
                    let n = parts.len();
                    if let Ok(col) = self
                        .ctx
                        .resolve_qualified(&parts[n - 2].value, &parts[n - 1].value)
                    {
                        return Ok(col.column_def.data_type.clone());
                    }
                }

                // Third try: just the last identifier
                if let Some(last) = parts.last() {
                    if let Ok(col) = self.ctx.resolve_column(&last.value) {
                        return Ok(col.column_def.data_type.clone());
                    }
                }

                Ok(DataType::Text)
            }

            Expr::Value(val) => Ok(self.infer_value(val)),

            Expr::Cast { data_type, .. } => Ok(sql_datatype_to_internal(data_type)),
            Expr::TypedString { data_type, .. } => Ok(sql_datatype_to_internal(data_type)),

            Expr::Function(f) => self.infer_function(f),

            Expr::BinaryOp { left, op, right } => {
                let left_type = self.infer(left)?;
                let right_type = self.infer(right)?;
                let op_str = format!("{:?}", op);
                binary_op_result_type(&op_str, &left_type, &right_type).ok_or_else(|| {
                    TypeError::OperatorTypeMismatch {
                        operator: op_str,
                        left: left_type,
                        right: right_type,
                    }
                })
            }

            Expr::UnaryOp { op, expr } => {
                use sqlparser::ast::UnaryOperator;
                match op {
                    UnaryOperator::Not => Ok(DataType::Boolean),
                    UnaryOperator::Plus | UnaryOperator::Minus => self.infer(expr),
                    _ => self.infer(expr),
                }
            }

            Expr::Case {
                operand: _,
                conditions: _,
                results,
                else_result,
            } => {
                let mut types = Vec::new();
                for result in results {
                    types.push(self.infer(result)?);
                }
                if let Some(else_expr) = else_result {
                    types.push(self.infer(else_expr)?);
                }
                unify_types(&types).ok_or_else(|| TypeError::CaseBranchTypeMismatch { types })
            }

            Expr::Subquery(query) => self.infer_subquery(query),
            Expr::InSubquery { .. } => Ok(DataType::Boolean),
            Expr::Exists { .. } => Ok(DataType::Boolean),

            Expr::Between { .. } => Ok(DataType::Boolean),
            Expr::Like { .. } => Ok(DataType::Boolean),
            Expr::ILike { .. } => Ok(DataType::Boolean),
            Expr::InList { .. } => Ok(DataType::Boolean),
            Expr::IsNull(_) => Ok(DataType::Boolean),
            Expr::IsNotNull(_) => Ok(DataType::Boolean),
            Expr::IsTrue(_) => Ok(DataType::Boolean),
            Expr::IsFalse(_) => Ok(DataType::Boolean),
            Expr::IsUnknown(_) => Ok(DataType::Boolean),

            Expr::Array(arr) => {
                if arr.elem.is_empty() {
                    Ok(DataType::Array(Box::new(DataType::Text)))
                } else {
                    let elem_type = self.infer(&arr.elem[0])?;
                    Ok(DataType::Array(Box::new(elem_type)))
                }
            }
            Expr::ArrayIndex { obj, .. } => {
                let arr_type = self.infer(obj)?;
                match arr_type {
                    DataType::Array(inner) => Ok(*inner),
                    _ => Ok(DataType::Text),
                }
            }

            Expr::JsonAccess { operator, .. } => {
                use sqlparser::ast::JsonOperator;
                match operator {
                    JsonOperator::Arrow | JsonOperator::HashArrow => Ok(DataType::Jsonb),
                    JsonOperator::LongArrow | JsonOperator::HashLongArrow => Ok(DataType::Text),
                    JsonOperator::AtArrow | JsonOperator::ArrowAt => Ok(DataType::Boolean),
                    _ => Ok(DataType::Text),
                }
            }

            Expr::AtTimeZone { timestamp, .. } => {
                let ts_type = self.infer(timestamp)?;
                match ts_type {
                    DataType::TimestampTz => Ok(DataType::Timestamp),
                    _ => Ok(DataType::TimestampTz),
                }
            }
            Expr::Interval(_) => Ok(DataType::Interval),
            Expr::Extract { .. } => Ok(DataType::Float64),

            Expr::Substring { expr, .. } => {
                let inner_type = self.infer(expr)?;
                match inner_type {
                    DataType::Bytes => Ok(DataType::Bytes),
                    _ => Ok(DataType::Text),
                }
            }
            Expr::Overlay { expr, .. } => {
                let inner_type = self.infer(expr)?;
                match inner_type {
                    DataType::Bytes => Ok(DataType::Bytes),
                    _ => Ok(DataType::Text),
                }
            }
            Expr::Trim { .. } => Ok(DataType::Text),
            Expr::Position { .. } => Ok(DataType::Int32),

            Expr::Nested(inner) => self.infer(inner),

            _ => Ok(DataType::Text),
        }
    }

    fn infer_value(&self, val: &sqlparser::ast::Value) -> DataType {
        use sqlparser::ast::Value as SqlValue;
        match val {
            SqlValue::Number(n, _) => {
                if n.contains(['e', 'E']) {
                    DataType::Float64
                } else if n.contains('.') {
                    DataType::Numeric {
                        precision: None,
                        scale: None,
                    }
                } else if n.parse::<i32>().is_ok() {
                    DataType::Int32
                } else {
                    DataType::Int64
                }
            }
            SqlValue::SingleQuotedString(_)
            | SqlValue::DoubleQuotedString(_)
            | SqlValue::EscapedStringLiteral(_) => DataType::Text,
            SqlValue::Boolean(_) => DataType::Boolean,
            SqlValue::Null => DataType::Text,
            SqlValue::HexStringLiteral(_) => DataType::Bytes,
            _ => DataType::Text,
        }
    }

    fn infer_function(&mut self, f: &Function) -> Result<DataType, TypeError> {
        let func_name = f
            .name
            .0
            .last()
            .map(|n| n.value.to_uppercase())
            .unwrap_or_default();

        let arg_types: Vec<DataType> = f
            .args
            .iter()
            .filter_map(|arg| match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => self.infer(expr).ok(),
                FunctionArg::Named {
                    arg: FunctionArgExpr::Expr(expr),
                    ..
                } => self.infer(expr).ok(),
                FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => Some(DataType::Int64),
                _ => None,
            })
            .collect();

        let registry = global_registry();
        if let Some(return_type) = registry.resolve_return_type(&func_name, &arg_types) {
            return Ok(return_type);
        }

        // Unknown function - return conservative default
        Ok(DataType::Text)
    }

    fn infer_subquery(&mut self, query: &Query) -> Result<DataType, TypeError> {
        let select = match &*query.body {
            SetExpr::Select(s) => s,
            _ => return Ok(DataType::Text),
        };

        if select.projection.len() != 1 {
            return Err(TypeError::ScalarSubqueryMultipleColumns);
        }

        match &select.projection[0] {
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                self.infer(expr)
            }
            SelectItem::Wildcard(_) => Ok(DataType::Text),
            _ => Ok(DataType::Text),
        }
    }
}

pub fn sql_datatype_to_internal(dt: &SqlDataType) -> DataType {
    match dt {
        SqlDataType::Boolean | SqlDataType::Bool => DataType::Boolean,
        SqlDataType::SmallInt(_) | SqlDataType::Int2(_) => DataType::Int32,
        SqlDataType::Int(_) | SqlDataType::Integer(_) | SqlDataType::Int4(_) => DataType::Int32,
        SqlDataType::BigInt(_) | SqlDataType::Int8(_) => DataType::Int64,
        SqlDataType::Real | SqlDataType::Float4 => DataType::Float64,
        SqlDataType::Double
        | SqlDataType::DoublePrecision
        | SqlDataType::Float8
        | SqlDataType::Float(_) => DataType::Float64,
        SqlDataType::Numeric(info) | SqlDataType::Decimal(info) => {
            use sqlparser::ast::ExactNumberInfo;
            let (precision, scale) = match info {
                ExactNumberInfo::None => (None, None),
                ExactNumberInfo::Precision(p) => (Some(*p as u32), Some(0)),
                ExactNumberInfo::PrecisionAndScale(p, s) => (Some(*p as u32), Some(*s as u32)),
            };
            DataType::Numeric { precision, scale }
        }
        SqlDataType::Varchar(_)
        | SqlDataType::Char(_)
        | SqlDataType::Text
        | SqlDataType::String(_) => DataType::Text,
        SqlDataType::Bytea => DataType::Bytes,
        SqlDataType::Timestamp(_, tz) => match tz {
            sqlparser::ast::TimezoneInfo::WithTimeZone | sqlparser::ast::TimezoneInfo::Tz => {
                DataType::TimestampTz
            }
            _ => DataType::Timestamp,
        },
        SqlDataType::Date => DataType::Date,
        SqlDataType::Time(_, _) => DataType::Time,
        SqlDataType::Interval => DataType::Interval,
        SqlDataType::Uuid => DataType::Uuid,
        SqlDataType::JSON => DataType::Jsonb,
        SqlDataType::Array(inner) => {
            use sqlparser::ast::ArrayElemTypeDef;
            match inner {
                ArrayElemTypeDef::AngleBracket(inner_type) => {
                    DataType::Array(Box::new(sql_datatype_to_internal(inner_type)))
                }
                ArrayElemTypeDef::SquareBracket(inner_type) => {
                    DataType::Array(Box::new(sql_datatype_to_internal(inner_type)))
                }
                ArrayElemTypeDef::None => DataType::Array(Box::new(DataType::Text)),
            }
        }
        SqlDataType::Custom(name, _) => {
            let name_str = name
                .0
                .iter()
                .map(|i| i.value.as_str())
                .collect::<Vec<_>>()
                .join(".");
            match name_str.to_uppercase().as_str() {
                "SERIAL" => DataType::Int32,
                "BIGSERIAL" => DataType::Int64,
                "TIMESTAMPTZ" => DataType::TimestampTz,
                "JSONB" => DataType::Jsonb,
                "JSON" => DataType::Json,
                "TSVECTOR" => DataType::Tsvector,
                "TSQUERY" => DataType::Tsquery,
                _ if name_str.to_uppercase().starts_with("VECTOR") => DataType::Vector(0),
                _ => DataType::UserDefined(name_str),
            }
        }
        _ => DataType::Text,
    }
}
