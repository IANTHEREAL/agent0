use super::*;
use crate::types::{ColumnDef, DataType, TableSchema};

fn int_col(name: &str) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type: DataType::Int32,
        nullable: true,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
        collation: None,
    }
}

fn text_col(name: &str) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type: DataType::Text,
        nullable: true,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
        collation: None,
    }
}

fn make_schema(columns: Vec<ColumnDef>) -> TableSchema {
    TableSchema {
        name: "test".to_string(),
        columns,
        ..Default::default()
    }
}

#[test]
fn test_column_resolution_single_table() {
    let schema = make_schema(vec![int_col("id"), text_col("name")]);
    let ctx = TypeContext::single(&schema);

    assert!(ctx.resolve_column("id").is_ok());
    assert!(ctx.resolve_column("name").is_ok());
    assert!(ctx.resolve_column("nonexistent").is_err());
}

#[test]
fn test_column_resolution_case_insensitive() {
    let schema = make_schema(vec![int_col("UserId")]);
    let ctx = TypeContext::single(&schema);

    assert!(ctx.resolve_column("userid").is_ok());
    assert!(ctx.resolve_column("USERID").is_ok());
    assert!(ctx.resolve_column("UserId").is_ok());
}

#[test]
fn test_column_resolution_join_ambiguous() {
    let left = make_schema(vec![int_col("id")]);
    let right = make_schema(vec![int_col("id")]);
    let ctx = TypeContext::join("a", &left, "b", &right);

    assert!(matches!(
        ctx.resolve_column("id"),
        Err(TypeError::AmbiguousColumn { .. })
    ));
    assert!(ctx.resolve_qualified("a", "id").is_ok());
    assert!(ctx.resolve_qualified("b", "id").is_ok());
}

#[test]
fn test_column_resolution_join_unique() {
    let left = make_schema(vec![int_col("id")]);
    let right = make_schema(vec![text_col("name")]);
    let ctx = TypeContext::join("a", &left, "b", &right);

    assert!(ctx.resolve_column("id").is_ok());
    assert!(ctx.resolve_column("name").is_ok());
}

#[test]
fn test_function_registry_count() {
    let reg = global_registry();
    assert_eq!(
        reg.resolve_return_type("COUNT", &[DataType::Int32]),
        Some(DataType::Int64)
    );
    assert_eq!(reg.resolve_return_type("count", &[]), Some(DataType::Int64));
}

#[test]
fn test_function_registry_sum() {
    let reg = global_registry();
    assert_eq!(
        reg.resolve_return_type("SUM", &[DataType::Int32]),
        Some(DataType::Int64)
    );
    assert_eq!(
        reg.resolve_return_type("SUM", &[DataType::Float64]),
        Some(DataType::Float64)
    );
}

#[test]
fn test_function_registry_min_max() {
    let reg = global_registry();
    assert_eq!(
        reg.resolve_return_type("MIN", &[DataType::Text]),
        Some(DataType::Text)
    );
    assert_eq!(
        reg.resolve_return_type("MAX", &[DataType::Int64]),
        Some(DataType::Int64)
    );
}

#[test]
fn test_type_unification() {
    let types = vec![DataType::Int32, DataType::Int64];
    assert_eq!(unify_types(&types), Some(DataType::Int64));

    let types = vec![DataType::Int32, DataType::Float64];
    assert_eq!(unify_types(&types), Some(DataType::Float64));

    let types = vec![DataType::Text, DataType::Int32];
    assert_eq!(unify_types(&types), Some(DataType::Text));
}

#[test]
fn test_binary_op_types() {
    assert_eq!(
        binary_op_result_type("Plus", &DataType::Int32, &DataType::Int64),
        Some(DataType::Int64)
    );
    assert_eq!(
        binary_op_result_type("Minus", &DataType::Timestamp, &DataType::Timestamp),
        Some(DataType::Interval)
    );
    assert_eq!(
        binary_op_result_type("Eq", &DataType::Text, &DataType::Text),
        Some(DataType::Boolean)
    );
}

#[test]
fn test_is_numeric() {
    assert!(is_numeric(&DataType::Int32));
    assert!(is_numeric(&DataType::Int64));
    assert!(is_numeric(&DataType::Float64));
    assert!(is_numeric(&DataType::Numeric {
        precision: None,
        scale: None
    }));
    assert!(!is_numeric(&DataType::Text));
    assert!(!is_numeric(&DataType::Boolean));
}

#[test]
fn test_window_function_sum_returns_numeric() {
    use sqlparser::ast::{Function, Ident, ObjectName, WindowSpec, WindowType};

    let schema = make_schema(vec![int_col("amount")]);
    let ctx = TypeContext::single(&schema);
    let mut inferrer = infer::TypeInferrer::new(ctx);

    // Window SUM should return Numeric
    let window_sum = Function {
        name: ObjectName(vec![Ident::new("SUM")]),
        args: vec![],
        over: Some(WindowType::WindowSpec(WindowSpec {
            partition_by: vec![],
            order_by: vec![],
            window_frame: None,
        })),
        filter: None,
        null_treatment: None,
        distinct: false,
        special: false,
        order_by: vec![],
    };

    let result = inferrer.infer(&sqlparser::ast::Expr::Function(window_sum));
    assert_eq!(
        result.unwrap(),
        DataType::Numeric {
            precision: None,
            scale: None
        }
    );
}

#[test]
fn test_window_function_avg_returns_numeric() {
    use sqlparser::ast::{Function, Ident, ObjectName, WindowSpec, WindowType};

    let schema = make_schema(vec![int_col("amount")]);
    let ctx = TypeContext::single(&schema);
    let mut inferrer = infer::TypeInferrer::new(ctx);

    // Window AVG should return Numeric
    let window_avg = Function {
        name: ObjectName(vec![Ident::new("AVG")]),
        args: vec![],
        over: Some(WindowType::WindowSpec(WindowSpec {
            partition_by: vec![],
            order_by: vec![],
            window_frame: None,
        })),
        filter: None,
        null_treatment: None,
        distinct: false,
        special: false,
        order_by: vec![],
    };

    let result = inferrer.infer(&sqlparser::ast::Expr::Function(window_avg));
    assert_eq!(
        result.unwrap(),
        DataType::Numeric {
            precision: None,
            scale: None
        }
    );
}

#[test]
fn test_aggregate_function_sum_uses_registry() {
    use sqlparser::ast::{Function, Ident, ObjectName};

    let schema = make_schema(vec![int_col("amount")]);
    let ctx = TypeContext::single(&schema);
    let mut inferrer = infer::TypeInferrer::new(ctx);

    // Regular (non-window) SUM should use registry logic (Int32 -> Int64)
    let agg_sum = Function {
        name: ObjectName(vec![Ident::new("SUM")]),
        args: vec![sqlparser::ast::FunctionArg::Unnamed(
            sqlparser::ast::FunctionArgExpr::Expr(sqlparser::ast::Expr::Identifier(Ident::new(
                "amount",
            ))),
        )],
        over: None, // No window clause
        filter: None,
        null_treatment: None,
        distinct: false,
        special: false,
        order_by: vec![],
    };

    let result = inferrer.infer(&sqlparser::ast::Expr::Function(agg_sum));
    // For aggregate SUM(int32), registry returns Int64
    assert_eq!(result.unwrap(), DataType::Int64);
}
