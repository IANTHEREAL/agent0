//! Function, CASE, and conditional expression analysis.
//!
//! Contains `analyze_case`, `analyze_function`, `analyze_coalesce`,
//! `analyze_nullif`, `analyze_greatest_least`, window spec helpers,
//! and `make_function_call`.

use sqlparser::ast::{self as ast, Expr, Function, FunctionArg, FunctionArgExpr, WindowType};

use crate::model::{DataType, Value};
use crate::sql::embedding_options::parse_embed_text_json_options_dimensions;
use crate::sql::expr::static_eval::eval_static_typed_expr;
use crate::sql::names::function_name_upper;
use crate::sql::query_context::QueryContext;
use crate::sql::types::coercion::comparison_target_type;
use crate::sql::types::registry::global_registry;

use crate::sql::analyzer::error::AnalyzerError;
use crate::sql::analyzer::types::*;
use crate::sql::analyzer::Analyzer;

/// Check whether `arg_type` is implicitly compatible with `target` for function
/// argument matching, mirroring PostgreSQL's implicit cast rules:
///   - Text family: Text, Varchar, Name, Char are compatible with Text
///   - Numeric widening: Int32/Int64 widen to Float64
///   - Json family: Json is compatible with Jsonb
fn is_implicitly_compatible(arg_type: &DataType, target: &DataType) -> bool {
    if arg_type == target {
        return true;
    }
    match target {
        DataType::Text => matches!(
            arg_type,
            DataType::Text | DataType::Varchar(_) | DataType::Name
        ),
        DataType::Float64 => matches!(
            arg_type,
            DataType::Float64 | DataType::Int32 | DataType::Int64 | DataType::Oid
        ),
        DataType::Int64 | DataType::Oid => {
            matches!(arg_type, DataType::Int64 | DataType::Int32 | DataType::Oid)
        }
        DataType::Jsonb => matches!(arg_type, DataType::Jsonb | DataType::Json),
        _ => false,
    }
}

fn is_two_arg_advisory_lock_function(name: &str) -> bool {
    matches!(
        name,
        "PG_ADVISORY_LOCK"
            | "PG_ADVISORY_LOCK_SHARED"
            | "PG_ADVISORY_XACT_LOCK"
            | "PG_ADVISORY_XACT_LOCK_SHARED"
            | "PG_TRY_ADVISORY_LOCK"
            | "PG_TRY_ADVISORY_LOCK_SHARED"
            | "PG_TRY_ADVISORY_XACT_LOCK"
            | "PG_TRY_ADVISORY_XACT_LOCK_SHARED"
            | "PG_ADVISORY_UNLOCK"
            | "PG_ADVISORY_UNLOCK_SHARED"
    )
}

fn function_schema_name(func: &Function) -> Option<String> {
    (func.name.0.len() > 1).then(|| crate::sql::names::normalize_ident(&func.name.0[0]))
}

fn function_qualified_name(func: &Function) -> String {
    func.name
        .0
        .iter()
        .map(crate::sql::names::normalize_ident)
        .collect::<Vec<_>>()
        .join(".")
}

/// Map an analyzed argument's type for use in `FunctionNotFound` error messages.
///
/// PostgreSQL reports untyped string literals and bare NULLs as `unknown` in
/// "function f(unknown) does not exist" diagnostics.  The analyzer has already
/// resolved these to `Text` / the inferred type, so we need to undo that
/// mapping when building the error signature.
fn error_display_arg_type(arg: &TypedExpr) -> DataType {
    if matches!(
        &arg.kind,
        TypedExprKind::Constant(Value::Text(_)) | TypedExprKind::Constant(Value::Null)
    ) {
        return DataType::UserDefined("unknown".to_string());
    }
    arg.data_type.clone()
}

fn static_value(expr: &TypedExpr) -> Option<Value> {
    match &expr.kind {
        TypedExprKind::Constant(value) => Some(value.clone()),
        _ => eval_static_typed_expr(expr, &QueryContext::from_task_locals()).ok(),
    }
}

fn constant_u32(expr: &TypedExpr) -> Option<u32> {
    match static_value(expr)? {
        Value::Int32(v) if v > 0 => Some(v as u32),
        Value::Int64(v) if v > 0 && v <= u32::MAX as i64 => Some(v as u32),
        Value::Text(v) => v.trim().parse::<u32>().ok().filter(|dim| *dim > 0),
        _ => None,
    }
}

fn embed_text_dimensions(expr: &TypedExpr) -> Result<Option<u32>, AnalyzerError> {
    match static_value(expr) {
        Some(Value::Text(opts)) => parse_embed_text_json_options_dimensions(&opts)
            .map_err(|message| AnalyzerError::InvalidParameterValue { message }),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Ok(None),
    }
}

fn refine_function_return_type(
    func_name: &str,
    args: &[TypedExpr],
    return_type: DataType,
) -> Result<DataType, AnalyzerError> {
    match func_name {
        "EMBEDDING" => Ok(args
            .get(2)
            .and_then(constant_u32)
            .map(DataType::Vector)
            .unwrap_or(return_type)),
        "EMBED_TEXT" => Ok(args
            .get(2)
            .map(embed_text_dimensions)
            .transpose()?
            .flatten()
            .map(DataType::Vector)
            .unwrap_or(return_type)),
        _ => Ok(return_type),
    }
}

impl<'a> Analyzer<'a> {
    fn validate_pg_catalog_only_function_qualification(
        &self,
        func: &Function,
        func_name: &str,
        analyzed_args: &[TypedExpr],
    ) -> Result<(), AnalyzerError> {
        if func.name.0.len() >= 3 {
            return Err(AnalyzerError::CrossDatabaseReference(
                function_qualified_name(func),
            ));
        }

        let Some(schema) = function_schema_name(func) else {
            return Ok(());
        };
        if schema == "pg_catalog" {
            return Ok(());
        }
        if self.catalog.schema_exists(&schema) {
            return Err(AnalyzerError::FunctionNotFound {
                name: format!("{}.{}", schema, func_name.to_lowercase()),
                arg_types: analyzed_args.iter().map(error_display_arg_type).collect(),
            });
        }
        Err(AnalyzerError::SchemaNotFound(schema))
    }

    pub(in crate::sql::analyzer) fn validate_no_positional_after_named(
        &self,
        args: &[FunctionArg],
    ) -> Result<(), AnalyzerError> {
        let mut seen_named = false;
        for arg in args {
            match arg {
                FunctionArg::Named { .. } => seen_named = true,
                FunctionArg::Unnamed(_) if seen_named => {
                    return Err(AnalyzerError::SqlStructure(
                        "positional argument cannot follow named argument".to_string(),
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }

    // -- Helper: CASE --

    pub(super) fn analyze_case(
        &mut self,
        operand: &Option<Box<Expr>>,
        conditions: &[Expr],
        results: &[Expr],
        else_result: &Option<Box<Expr>>,
    ) -> Result<TypedExpr, AnalyzerError> {
        let mut analyzed_operand = match operand {
            Some(e) => Some(Box::new(self.analyze_expr(e)?)),
            None => None,
        };

        let mut when_clauses = Vec::with_capacity(conditions.len());

        for (cond, result) in conditions.iter().zip(results.iter()) {
            let mut c = self.analyze_expr(cond)?;
            // For searched CASE (no operand), each WHEN condition must be boolean.
            // For simple CASE (with operand), conditions are values compared to
            // the operand, so any type is valid.
            if analyzed_operand.is_none() {
                if c.is_null_constant() {
                    c = TypedExpr::null(DataType::Boolean);
                } else if let TypedExprKind::Parameter { index } = &c.kind {
                    // Parameter in searched CASE WHEN -> resolve to Boolean
                    let was_unresolved = self.is_unresolved_param(&c);
                    self.resolve_param_type(*index, &DataType::Boolean)?;
                    if was_unresolved {
                        c = TypedExpr::new(
                            TypedExprKind::Parameter { index: *index },
                            DataType::Boolean,
                        );
                    }
                } else if matches!(c.kind, TypedExprKind::Constant(Value::Text(_))) {
                    // PostgreSQL unknown-literal behavior in boolean context:
                    // CASE WHEN 'true' THEN ... is allowed via implicit cast.
                    c = self.coerce_if_needed(c, &DataType::Boolean)?;
                }

                if c.data_type != DataType::Boolean {
                    return Err(AnalyzerError::TypeMismatch {
                        expected: DataType::Boolean,
                        found: c.data_type.clone(),
                        context: "CASE WHEN condition".to_string(),
                    });
                }
            }
            let r = self.analyze_expr(result)?;
            when_clauses.push((c, r));
        }

        // Simple CASE: coerce operand and WHEN values to a single comparison target type.
        //
        // PostgreSQL desugars `CASE operand WHEN v THEN ...` into comparisons
        // (`operand = v`) with coercion. The Typed IR must not rely on runtime
        // comparison coercion; insert casts here so executor evaluation only
        // compares type-compatible values.
        if let Some(op) = analyzed_operand.take() {
            let mut target = op.data_type.clone();
            for (when_expr, _) in &when_clauses {
                target =
                    comparison_target_type(&target, &when_expr.data_type).ok_or_else(|| {
                        AnalyzerError::OperatorTypeMismatch {
                            operator: "=".to_string(),
                            left: target.pg_display_name(),
                            right: when_expr.data_type.pg_display_name(),
                        }
                    })?;
            }

            analyzed_operand = Some(Box::new(self.coerce_if_needed(*op, &target)?));
            when_clauses = when_clauses
                .into_iter()
                .map(|(cond, result)| Ok((self.coerce_if_needed(cond, &target)?, result)))
                .collect::<Result<_, AnalyzerError>>()?;
        }

        let analyzed_else = match else_result {
            Some(e) => Some(Box::new(self.analyze_expr(e)?)),
            None => None,
        };

        // Collect result expression refs for NULL-aware type unification.
        let result_refs: Vec<&TypedExpr> = when_clauses
            .iter()
            .map(|(_, r)| r)
            .chain(analyzed_else.iter().map(|e| &**e))
            .collect();
        let result_type = self.unify_expr_types(&result_refs, "CASE")?;

        // Insert implicit casts on result expressions to match the unified type.
        let when_clauses = when_clauses
            .into_iter()
            .map(|(cond, result)| Ok((cond, self.coerce_if_needed(result, &result_type)?)))
            .collect::<Result<_, AnalyzerError>>()?;
        let analyzed_else = match analyzed_else {
            Some(e) => Some(Box::new(self.coerce_if_needed(*e, &result_type)?)),
            None => None,
        };

        Ok(TypedExpr::new(
            TypedExprKind::Case {
                operand: analyzed_operand,
                when_clauses,
                else_result: analyzed_else,
            },
            result_type,
        ))
    }

    // -- Helper: function analysis --

    pub(super) fn analyze_function(&mut self, func: &Function) -> Result<TypedExpr, AnalyzerError> {
        self.validate_no_positional_after_named(&func.args)?;

        let mut func_name = function_name_upper(func);

        // Extract schema qualifier for schema-qualified function calls (e.g.,
        // cron.schedule, swarm.my_func). function_name_upper() only takes the
        // last segment, so we need to reconstruct the qualified name for both
        // builtin dispatch and UDF catalog resolution.
        let func_schema: Option<String> = if func.name.0.len() > 1 {
            Some(func.name.0[0].value.to_lowercase())
        } else {
            None
        };

        // Preserve schema prefix in func_name for builtin registry dispatch
        // (cron.*, auth.* have schema-qualified entries in the registry).
        // For UDFs, the schema is passed separately to resolve_function().
        if let Some(ref schema) = func_schema {
            if schema == "cron" || schema == "auth" || schema == "serverless_functions" {
                func_name = format!("{}.{}", schema, func_name);
            }
        }

        // Extract function arguments
        let args = self.extract_function_args(func)?;

        // Analyze argument expressions
        let analyzed_args: Vec<TypedExpr> = args
            .iter()
            .map(|a| self.analyze_expr(a))
            .collect::<Result<_, _>>()?;
        // PostgreSQL builtin functions like pg_get_serial_sequence, to_regclass,
        // and to_regtype only exist in pg_catalog. Schema-qualified validation
        // must run before argument-context coercion so error signatures preserve
        // the original qualified function name.
        if matches!(
            func_name.as_str(),
            "PG_GET_SERIAL_SEQUENCE" | "TO_REGCLASS" | "TO_REGTYPE"
        ) {
            self.validate_pg_catalog_only_function_qualification(func, &func_name, &analyzed_args)?;
        }

        let analyzed_args =
            self.apply_function_arg_context(func_name.as_str(), analyzed_args, func)?;

        let arg_types: Vec<DataType> = analyzed_args.iter().map(|a| a.data_type.clone()).collect();

        // Intercept conditional expressions that need dedicated IR variants.
        // sqlparser 0.40 parses COALESCE/NULLIF/GREATEST/LEAST as Expr::Function,
        // but they have special short-circuit semantics requiring dedicated IR nodes.
        match func_name.as_str() {
            "COALESCE" => return self.analyze_coalesce(analyzed_args),
            "NULLIF" => return self.analyze_nullif(analyzed_args),
            "GREATEST" => return self.analyze_greatest_least(analyzed_args, true),
            "LEAST" => return self.analyze_greatest_least(analyzed_args, false),
            // ROW(...) constructor is represented as a dedicated Typed IR node.
            "ROW" => {
                return Ok(TypedExpr::new(
                    TypedExprKind::Row(analyzed_args),
                    DataType::UserDefined("record".to_string()),
                ))
            }
            _ => {}
        }

        // PostgreSQL polymorphic resolution: TO_JSONB(unknown_literal) fails unless
        // the literal is explicitly typed/cast.
        if func_name == "TO_JSONB"
            && args.len() == 1
            && matches!(args[0], Expr::Value(ast::Value::SingleQuotedString(_)))
        {
            return Err(AnalyzerError::Unsupported(
                "could not determine polymorphic type because input has type unknown".to_string(),
            ));
        }

        // Analyze FILTER clause
        let filter = match &func.filter {
            Some(f) => Some(Box::new(self.analyze_expr(f)?)),
            None => None,
        };

        // Analyze ORDER BY within function
        let order_by = if func.order_by.is_empty() {
            vec![]
        } else {
            self.analyze_order_by_exprs(&func.order_by, &[])?
        };

        // Resolve function from registry
        let registry = global_registry();

        if let Some(sig) = registry.get(&func_name) {
            // Validate argument count — PG treats arity mismatch as
            // "function name(arg_types) does not exist" (SQLSTATE 42883).
            let arg_count = analyzed_args.len();
            if arg_count < sig.min_args || sig.max_args.is_some_and(|max| arg_count > max) {
                return Err(AnalyzerError::FunctionNotFound {
                    name: func_name.to_lowercase(),
                    arg_types: analyzed_args.iter().map(error_display_arg_type).collect(),
                });
            }

            // quote_ident() only accepts text-like types (text, varchar, name, unknown).
            // PG rejects non-text arguments with SQLSTATE 42883.
            if func_name == "QUOTE_IDENT" {
                if let Some(arg_type) = arg_types.first() {
                    if !matches!(
                        arg_type,
                        DataType::Text | DataType::Varchar(_) | DataType::Name | DataType::Unknown
                    ) {
                        return Err(AnalyzerError::FunctionNotFound {
                            name: func_name.to_lowercase(),
                            arg_types: arg_types.clone(),
                        });
                    }
                }
            }

            // json_object_keys, json_array_elements, json_array_elements_text only
            // accept json, not jsonb.  PostgreSQL rejects jsonb at function resolution
            // (SQLSTATE 42883), not at execution time.
            if matches!(
                func_name.as_str(),
                "JSON_OBJECT_KEYS" | "JSON_ARRAY_ELEMENTS" | "JSON_ARRAY_ELEMENTS_TEXT"
            ) {
                if let Some(DataType::Jsonb) = arg_types.first() {
                    return Err(AnalyzerError::FunctionNotFound {
                        name: func_name.to_lowercase(),
                        arg_types: analyzed_args.iter().map(error_display_arg_type).collect(),
                    });
                }
            }

            // Reject window-only functions used without OVER clause.
            // Functions like ROW_NUMBER(), RANK() are meaningless without a window.
            if sig.is_window && !sig.is_aggregate && func.over.is_none() {
                return Err(AnalyzerError::WindowNotAllowed {
                    function: func_name,
                    context: "called without OVER clause".to_string(),
                });
            }

            // Resolve return type using the signature's ReturnType resolver.
            // None here means the argument types don't match the resolver
            // (e.g., SameAsArg(0) with no args after arg count validation
            // passed -- indicates a registry bug). Treat as error, not silent
            // fallback, to surface misconfigurations.
            let return_type = registry
                .resolve_return_type(&func_name, &arg_types)
                .ok_or_else(|| AnalyzerError::FunctionNotFound {
                    name: func_name.clone(),
                    arg_types: arg_types.clone(),
                })?;
            let return_type = refine_function_return_type(&func_name, &analyzed_args, return_type)?;

            let resolved = ResolvedFunction {
                name: func_name.clone(),
                kind: FunctionKind::Builtin,
                return_type: return_type.clone(),
            };

            // Determine expression kind based on function classification + OVER clause
            if func.over.is_some() {
                // Window functions are only allowed in SELECT, ORDER BY, and HAVING.
                if !self.scopes.current().allow_windows {
                    return Err(AnalyzerError::WindowNotAllowed {
                        function: func_name,
                        context: "WHERE clause or GROUP BY".to_string(),
                    });
                }
                // Window function
                let (partition_by, window_order_by, window_frame) =
                    self.analyze_window_spec(func)?;

                return Ok(TypedExpr::new(
                    TypedExprKind::WindowCall {
                        func: resolved,
                        args: analyzed_args,
                        partition_by,
                        order_by: window_order_by,
                        window_frame,
                    },
                    return_type,
                ));
            }

            if sig.is_aggregate {
                if !self.scopes.current().allow_aggregates {
                    return Err(AnalyzerError::AggregateNotAllowed {
                        function: func_name,
                        context: "WHERE clause or GROUP BY".to_string(),
                    });
                }
                return Ok(TypedExpr::new(
                    TypedExprKind::AggregateCall {
                        func: resolved,
                        args: analyzed_args,
                        distinct: func.distinct,
                        order_by,
                        filter,
                    },
                    return_type,
                ));
            }

            return Ok(TypedExpr::new(
                TypedExprKind::FunctionCall {
                    func: resolved,
                    args: analyzed_args,
                    order_by,
                    filter,
                },
                return_type,
            ));
        }

        // Not in builtin registry -- check catalog for UDF.
        // For schema-qualified calls (e.g. swarm.tmp()), pass the schema to
        // resolve_function so it looks up "schema.name" directly instead of
        // searching the search_path. For unqualified calls, schema is None
        // and the catalog falls through to search_path resolution.
        if let Ok(Some(func_def)) =
            self.catalog
                .resolve_function(&func_name, func_schema.as_deref(), &arg_types)
        {
            let return_type = self
                .resolve_sql_type_text(&func_def.return_type)
                .map_err(|e| {
                    AnalyzerError::Unsupported(format!(
                        "UDF {}: unsupported return type '{}': {}",
                        func_name, func_def.return_type, e
                    ))
                })?;

            // Store the fully qualified name (schema.name) in the IR so the
            // executor can resolve the function without relying on search_path.
            let resolved_name = if let Some(ref schema) = func_schema {
                format!("{}.{}", schema, func_name.to_lowercase())
            } else {
                func_name.clone()
            };

            let resolved = ResolvedFunction {
                name: resolved_name,
                kind: FunctionKind::UserDefined { oid: func_def.oid },
                return_type: return_type.clone(),
            };

            return Ok(TypedExpr::new(
                TypedExprKind::FunctionCall {
                    func: resolved,
                    args: analyzed_args,
                    order_by,
                    filter,
                },
                return_type,
            ));
        }

        // Unknown function -- treat as opaque call returning Text.
        // The runtime function registry (eval_expr) handles many pg-specific functions
        // that aren't registered in the type registry. Rather than hard-failing at
        // analysis time, we pass through and let runtime evaluation handle dispatch.
        let resolved = ResolvedFunction {
            name: func_name,
            kind: FunctionKind::Builtin,
            return_type: DataType::Text,
        };
        Ok(TypedExpr::new(
            TypedExprKind::FunctionCall {
                func: resolved,
                args: analyzed_args,
                order_by,
                filter,
            },
            DataType::Text,
        ))
    }

    /// Extract argument expressions from a function call, flattening named args.
    fn extract_function_args<'b>(
        &self,
        func: &'b Function,
    ) -> Result<Vec<&'b Expr>, AnalyzerError> {
        let mut exprs = Vec::new();
        for arg in &func.args {
            match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => exprs.push(e),
                FunctionArg::Named {
                    arg: FunctionArgExpr::Expr(e),
                    ..
                } => exprs.push(e),
                FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => {
                    // COUNT(*) -- no argument to analyze
                }
                _ => {}
            }
        }
        Ok(exprs)
    }

    fn apply_function_arg_context(
        &mut self,
        func_name: &str,
        args: Vec<TypedExpr>,
        func: &Function,
    ) -> Result<Vec<TypedExpr>, AnalyzerError> {
        match func_name {
            // Vector distance functions require vector arguments. This provides
            // parameter typing context for $N placeholders in extended protocol.
            "COSINE_DISTANCE" | "L2_DISTANCE" | "INNER_PRODUCT" => {
                self.coerce_args_to_vector(args, 2)
            }
            "VECTOR_DIMS" | "VECTOR_NORM" | "L2_NORMALIZE" => self.coerce_args_to_vector(args, 1),
            "GENERATE_SUBSCRIPTS" => self.coerce_generate_subscripts_signature(func_name, args),
            "PG_GET_INDEXDEF" => self.coerce_pg_get_indexdef_signature(args),
            "PG_GET_SERIAL_SEQUENCE" => {
                self.coerce_pg_get_serial_sequence_signature(func, func_name, args)
            }
            "TO_REGTYPE" | "TO_REGCLASS" => self.coerce_to_regtype_signature(func_name, args),
            "TO_TSVECTOR"
            | "PLAINTO_TSQUERY"
            | "PHRASETO_TSQUERY"
            | "TO_TSQUERY"
            | "WEBSEARCH_TO_TSQUERY" => self.coerce_fts_text_signature(func_name, args),
            "EMBEDDING" => self.coerce_embedding_signature(func_name, args),
            "EMBED_TEXT" => self.coerce_embed_text_signature(func_name, args),
            "VEC_EMBED_COSINE_DISTANCE" | "VEC_EMBED_L2_DISTANCE" | "VEC_EMBED_INNER_PRODUCT" => {
                self.coerce_vec_embed_signature(func_name, args)
            }
            "FS9_READ" | "FS9_WRITE" | "FS9_EXISTS" | "FS9_SIZE" | "FS9_MTIME" | "FS9_REMOVE"
            | "FS9_MKDIR" | "FS9_READ_AT" | "FS9_READ_BYTEA" | "FS9_READ_AT_BYTEA"
            | "FS9_WRITE_AT" | "FS9_APPEND" | "FS9_TRUNCATE" => {
                self.coerce_fs9_signature(func_name, args)
            }
            "HTTP_GET" | "HTTP_HEAD" | "HTTP_DELETE" | "HTTP_POST" | "HTTP_PUT" | "HTTP_PATCH"
            | "HTTP" => self.coerce_http_signature(func_name, args),
            "MAKE_INTERVAL" => self.reorder_make_interval_named_args(args, func),
            _ if is_two_arg_advisory_lock_function(func_name) => {
                self.coerce_advisory_lock_two_arg_signature(func_name, args)
            }
            _ => {
                // Generic fallback: use registry arg_types if available.
                // Covers ordinary single-signature functions like lower(text),
                // length(text), sqrt(float8) etc. for PREPARE parameter inference.
                let sig = global_registry().get(func_name);
                match sig {
                    Some(sig) if !sig.arg_types.is_empty() => {
                        self.coerce_registry_arg_types(func_name, args, &sig.arg_types)
                    }
                    _ => Ok(args),
                }
            }
        }
    }

    /// Reorder named arguments for MAKE_INTERVAL to positional order.
    ///
    /// PostgreSQL parameter order: years(0), months(1), weeks(2), days(3),
    /// hours(4), mins(5), secs(6). All default to 0. When named args are used,
    /// `extract_function_args` discards the names, so we must reorder here using
    /// the original `func.args` which still has the names.
    fn reorder_make_interval_named_args(
        &mut self,
        args: Vec<TypedExpr>,
        func: &Function,
    ) -> Result<Vec<TypedExpr>, AnalyzerError> {
        // If no named args, return as-is (pure positional or zero-arg call).
        let has_named = func
            .args
            .iter()
            .any(|a| matches!(a, FunctionArg::Named { .. }));
        if !has_named {
            return Ok(args);
        }

        // Parameter name → positional index.
        let param_index = |name: &str| -> Option<usize> {
            match name.to_lowercase().as_str() {
                "years" => Some(0),
                "months" => Some(1),
                "weeks" => Some(2),
                "days" => Some(3),
                "hours" => Some(4),
                "mins" => Some(5),
                "secs" => Some(6),
                _ => None,
            }
        };

        // Build a slot array of 7 positions, filled with defaults.
        let default_int = TypedExpr::new(TypedExprKind::Constant(Value::Int32(0)), DataType::Int32);
        let default_secs = TypedExpr::new(
            TypedExprKind::Constant(Value::Float64(0.0)),
            DataType::Float64,
        );
        let mut slots: [Option<TypedExpr>; 7] = Default::default();

        // The analyzed args are in the same order as func.args (names discarded).
        // Walk func.args to recover the mapping.
        let mut arg_idx = 0;
        for fa in &func.args {
            match fa {
                FunctionArg::Named { name, .. } => {
                    let param_name = name.value.as_str();
                    let slot = param_index(param_name).ok_or_else(|| {
                        AnalyzerError::SqlStructure(format!(
                            "make_interval: unknown parameter \"{}\"",
                            param_name
                        ))
                    })?;
                    if slot < slots.len() {
                        slots[slot] = Some(args[arg_idx].clone());
                    }
                    arg_idx += 1;
                }
                FunctionArg::Unnamed(FunctionArgExpr::Expr(_)) => {
                    // Positional args fill slots in order (only valid before any named arg,
                    // which is already validated by validate_no_positional_after_named).
                    if arg_idx < 7 {
                        slots[arg_idx] = Some(args[arg_idx].clone());
                    }
                    arg_idx += 1;
                }
                _ => {}
            }
        }

        // Fill defaults and collect.
        let result: Vec<TypedExpr> = slots
            .into_iter()
            .enumerate()
            .map(|(i, slot)| {
                slot.unwrap_or_else(|| {
                    if i == 6 {
                        default_secs.clone()
                    } else {
                        default_int.clone()
                    }
                })
            })
            .collect();

        Ok(result)
    }

    fn coerce_advisory_lock_two_arg_signature(
        &mut self,
        func_name: &str,
        args: Vec<TypedExpr>,
    ) -> Result<Vec<TypedExpr>, AnalyzerError> {
        if args.len() != 2 {
            return Ok(args);
        }

        let arg_types: Vec<DataType> = args.iter().map(|a| a.data_type.clone()).collect();
        let mut coerced = Vec::with_capacity(2);
        for arg in args {
            if arg.data_type == DataType::Int32 {
                coerced.push(arg);
                continue;
            }
            if self.is_unresolved_param(&arg) || arg.is_null_constant() {
                coerced.push(self.coerce_if_needed(arg, &DataType::Int32)?);
                continue;
            }
            return Err(AnalyzerError::FunctionNotFound {
                name: func_name.to_string(),
                arg_types,
            });
        }
        Ok(coerced)
    }

    /// Generic registry-based parameter coercion and type validation.
    ///
    /// Three cases per argument:
    /// 1. Unresolved parameter / NULL → coerce to declared arg type (PREPARE inference).
    /// 2. Pre-typed parameter (from PREPARE explicit type list) → validate compatibility,
    ///    reject mismatches (e.g. `PREPARE q(int) AS SELECT lower($1)` → error).
    /// 3. Non-parameter expression (column ref, literal, function call) → pass through.
    ///    The registry stores only one signature per function and does not model PG
    ///    overloads (e.g. `length(bytea)`, `quote_literal(anyelement)`), so we must
    ///    not reject non-parameter args that simply don't match the modeled signature.
    fn coerce_registry_arg_types(
        &mut self,
        func_name: &str,
        args: Vec<TypedExpr>,
        expected: &[DataType],
    ) -> Result<Vec<TypedExpr>, AnalyzerError> {
        let arg_types: Vec<DataType> = args.iter().map(|a| a.data_type.clone()).collect();
        let mut coerced = Vec::with_capacity(args.len());
        for (idx, arg) in args.into_iter().enumerate() {
            let Some(target) = expected.get(idx) else {
                coerced.push(arg);
                continue;
            };
            if self.is_unresolved_param(&arg)
                || arg.is_null_constant()
                || arg.data_type == DataType::Unknown
            {
                // Case 1: infer type for unresolved param or unknown-typed literal
                coerced.push(self.coerce_if_needed(arg, target)?);
            } else if matches!(arg.kind, TypedExprKind::Parameter { .. }) {
                // Case 2: pre-typed parameter — validate against declared type
                if is_implicitly_compatible(&arg.data_type, target) {
                    coerced.push(self.coerce_if_needed(arg, target)?);
                } else {
                    return Err(AnalyzerError::FunctionNotFound {
                        name: func_name.to_lowercase(),
                        arg_types,
                    });
                }
            } else {
                // Case 3: non-parameter — pass through (registry may not model all overloads)
                coerced.push(arg);
            }
        }
        Ok(coerced)
    }

    fn coerce_http_signature(
        &mut self,
        func_name: &str,
        args: Vec<TypedExpr>,
    ) -> Result<Vec<TypedExpr>, AnalyzerError> {
        // Expected arg types per position for each HTTP function group:
        //   GET/HEAD/DELETE: (url TEXT [, headers JSONB])
        //   POST/PUT/PATCH:  (url TEXT, body TEXT, content_type TEXT [, headers JSONB])
        //   HTTP (universal): (method TEXT, uri TEXT [, headers JSONB [, content_type TEXT [, content TEXT]]])
        let expected_types: &[DataType] = match func_name {
            "HTTP_GET" | "HTTP_HEAD" | "HTTP_DELETE" => &[DataType::Text, DataType::Jsonb],
            "HTTP_POST" | "HTTP_PUT" | "HTTP_PATCH" => &[
                DataType::Text,
                DataType::Text,
                DataType::Text,
                DataType::Jsonb,
            ],
            "HTTP" => &[
                DataType::Text,
                DataType::Text,
                DataType::Jsonb,
                DataType::Text,
                DataType::Text,
            ],
            _ => return Ok(args),
        };

        let arg_types: Vec<DataType> = args.iter().map(|a| a.data_type.clone()).collect();
        let mut coerced = Vec::with_capacity(args.len());
        for (idx, arg) in args.into_iter().enumerate() {
            let Some(target) = expected_types.get(idx) else {
                coerced.push(arg);
                continue;
            };
            let compatible = match target {
                DataType::Text => matches!(
                    arg.data_type,
                    DataType::Text | DataType::Varchar(_) | DataType::Name | DataType::Unknown
                ),
                DataType::Jsonb => matches!(arg.data_type, DataType::Jsonb | DataType::Json),
                _ => arg.data_type == *target,
            };
            if !compatible && !self.is_unresolved_param(&arg) && !arg.is_null_constant() {
                return Err(AnalyzerError::FunctionNotFound {
                    name: func_name.to_lowercase(),
                    arg_types,
                });
            }
            coerced.push(self.coerce_if_needed(arg, target)?);
        }
        Ok(coerced)
    }

    fn coerce_fs9_signature(
        &mut self,
        func_name: &str,
        args: Vec<TypedExpr>,
    ) -> Result<Vec<TypedExpr>, AnalyzerError> {
        match func_name {
            "FS9_READ" | "FS9_READ_BYTEA" | "FS9_EXISTS" | "FS9_SIZE" | "FS9_MTIME" => {
                self.coerce_fs9_path_only_signature(func_name, args)
            }
            "FS9_WRITE" | "FS9_APPEND" => self.coerce_fs9_write_signature(func_name, args),
            "FS9_REMOVE" | "FS9_MKDIR" => self.coerce_fs9_path_bool_signature(func_name, args),
            "FS9_READ_AT" | "FS9_READ_AT_BYTEA" => {
                self.coerce_fs9_read_at_signature(func_name, args)
            }
            "FS9_WRITE_AT" => self.coerce_fs9_write_at_signature(func_name, args),
            "FS9_TRUNCATE" => self.coerce_fs9_truncate_signature(func_name, args),
            _ => Ok(args),
        }
    }

    fn coerce_fs9_path_only_signature(
        &mut self,
        func_name: &str,
        args: Vec<TypedExpr>,
    ) -> Result<Vec<TypedExpr>, AnalyzerError> {
        if args.len() != 1 {
            return Ok(args);
        }
        let arg_types: Vec<DataType> = args.iter().map(|arg| arg.data_type.clone()).collect();
        let mut args = args.into_iter();
        let path =
            self.coerce_fs9_path_arg(func_name, args.next().expect("arity checked"), &arg_types)?;
        Ok(vec![path])
    }

    fn coerce_fs9_write_signature(
        &mut self,
        func_name: &str,
        args: Vec<TypedExpr>,
    ) -> Result<Vec<TypedExpr>, AnalyzerError> {
        if args.len() != 2 {
            return Ok(args);
        }
        let arg_types: Vec<DataType> = args.iter().map(|arg| arg.data_type.clone()).collect();
        let mut args = args.into_iter();
        let path =
            self.coerce_fs9_path_arg(func_name, args.next().expect("arity checked"), &arg_types)?;
        let data = self.coerce_fs9_file_data_arg(
            func_name,
            args.next().expect("arity checked"),
            &arg_types,
        )?;
        Ok(vec![path, data])
    }

    fn coerce_fs9_path_bool_signature(
        &mut self,
        func_name: &str,
        args: Vec<TypedExpr>,
    ) -> Result<Vec<TypedExpr>, AnalyzerError> {
        if args.is_empty() || args.len() > 2 {
            return Ok(args);
        }
        let arg_types: Vec<DataType> = args.iter().map(|arg| arg.data_type.clone()).collect();
        let mut coerced = Vec::with_capacity(args.len());
        for (idx, arg) in args.into_iter().enumerate() {
            let coerced_arg = match idx {
                0 => self.coerce_fs9_path_arg(func_name, arg, &arg_types)?,
                1 => self.coerce_fs9_boolean_arg(func_name, arg, &arg_types)?,
                _ => unreachable!("arity already validated"),
            };
            coerced.push(coerced_arg);
        }
        Ok(coerced)
    }

    fn coerce_fs9_read_at_signature(
        &mut self,
        func_name: &str,
        args: Vec<TypedExpr>,
    ) -> Result<Vec<TypedExpr>, AnalyzerError> {
        if args.len() != 3 {
            return Ok(args);
        }
        let arg_types: Vec<DataType> = args.iter().map(|arg| arg.data_type.clone()).collect();
        let mut args = args.into_iter();
        let path =
            self.coerce_fs9_path_arg(func_name, args.next().expect("arity checked"), &arg_types)?;
        let offset = self.coerce_fs9_integer_arg(
            func_name,
            args.next().expect("arity checked"),
            &arg_types,
        )?;
        let length = self.coerce_fs9_integer_arg(
            func_name,
            args.next().expect("arity checked"),
            &arg_types,
        )?;
        Ok(vec![path, offset, length])
    }

    fn coerce_fs9_write_at_signature(
        &mut self,
        func_name: &str,
        args: Vec<TypedExpr>,
    ) -> Result<Vec<TypedExpr>, AnalyzerError> {
        if args.len() != 3 {
            return Ok(args);
        }
        let arg_types: Vec<DataType> = args.iter().map(|arg| arg.data_type.clone()).collect();
        let mut args = args.into_iter();
        let path =
            self.coerce_fs9_path_arg(func_name, args.next().expect("arity checked"), &arg_types)?;
        let offset = self.coerce_fs9_integer_arg(
            func_name,
            args.next().expect("arity checked"),
            &arg_types,
        )?;
        let data = self.coerce_fs9_file_data_arg(
            func_name,
            args.next().expect("arity checked"),
            &arg_types,
        )?;
        Ok(vec![path, offset, data])
    }

    fn coerce_fs9_truncate_signature(
        &mut self,
        func_name: &str,
        args: Vec<TypedExpr>,
    ) -> Result<Vec<TypedExpr>, AnalyzerError> {
        if args.len() != 2 {
            return Ok(args);
        }
        let arg_types: Vec<DataType> = args.iter().map(|arg| arg.data_type.clone()).collect();
        let mut args = args.into_iter();
        let path =
            self.coerce_fs9_path_arg(func_name, args.next().expect("arity checked"), &arg_types)?;
        let size = self.coerce_fs9_integer_arg(
            func_name,
            args.next().expect("arity checked"),
            &arg_types,
        )?;
        Ok(vec![path, size])
    }

    fn coerce_fs9_path_arg(
        &mut self,
        func_name: &str,
        arg: TypedExpr,
        arg_types: &[DataType],
    ) -> Result<TypedExpr, AnalyzerError> {
        let arg_is_text_like = matches!(
            arg.data_type,
            DataType::Text | DataType::Varchar(_) | DataType::Name | DataType::Unknown
        );
        if !arg_is_text_like && !self.is_unresolved_param(&arg) && !arg.is_null_constant() {
            return Err(AnalyzerError::FunctionNotFound {
                name: func_name.to_lowercase(),
                arg_types: arg_types.to_vec(),
            });
        }
        self.coerce_if_needed(arg, &DataType::Text)
    }

    fn coerce_fs9_file_data_arg(
        &mut self,
        func_name: &str,
        arg: TypedExpr,
        arg_types: &[DataType],
    ) -> Result<TypedExpr, AnalyzerError> {
        if matches!(arg.data_type, DataType::Bytes) {
            return Ok(arg);
        }
        let arg_is_text_like = matches!(
            arg.data_type,
            DataType::Text | DataType::Varchar(_) | DataType::Name | DataType::Unknown
        );
        if !arg_is_text_like && !self.is_unresolved_param(&arg) && !arg.is_null_constant() {
            return Err(AnalyzerError::FunctionNotFound {
                name: func_name.to_lowercase(),
                arg_types: arg_types.to_vec(),
            });
        }
        self.coerce_if_needed(arg, &DataType::Text)
    }

    fn coerce_fs9_boolean_arg(
        &mut self,
        func_name: &str,
        arg: TypedExpr,
        arg_types: &[DataType],
    ) -> Result<TypedExpr, AnalyzerError> {
        if !matches!(arg.data_type, DataType::Boolean)
            && !self.is_unresolved_param(&arg)
            && !arg.is_null_constant()
        {
            return Err(AnalyzerError::FunctionNotFound {
                name: func_name.to_lowercase(),
                arg_types: arg_types.to_vec(),
            });
        }
        self.coerce_if_needed(arg, &DataType::Boolean)
    }

    fn coerce_fs9_integer_arg(
        &mut self,
        func_name: &str,
        arg: TypedExpr,
        arg_types: &[DataType],
    ) -> Result<TypedExpr, AnalyzerError> {
        if !matches!(
            arg.data_type,
            DataType::Int32 | DataType::Int64 | DataType::Oid
        ) && !self.is_unresolved_param(&arg)
            && !arg.is_null_constant()
        {
            return Err(AnalyzerError::FunctionNotFound {
                name: func_name.to_lowercase(),
                arg_types: arg_types.to_vec(),
            });
        }
        self.coerce_if_needed(arg, &DataType::Int64)
    }

    fn coerce_args_to_vector(
        &mut self,
        args: Vec<TypedExpr>,
        expected_arity: usize,
    ) -> Result<Vec<TypedExpr>, AnalyzerError> {
        if args.len() != expected_arity {
            return Ok(args);
        }

        let target = args
            .iter()
            .find_map(|arg| match &arg.data_type {
                DataType::Vector(dim) => Some(DataType::Vector(*dim)),
                _ => None,
            })
            .unwrap_or(DataType::Vector(0));

        args.into_iter()
            .map(|arg| self.coerce_if_needed(arg, &target))
            .collect()
    }

    fn coerce_generate_subscripts_signature(
        &mut self,
        func_name: &str,
        args: Vec<TypedExpr>,
    ) -> Result<Vec<TypedExpr>, AnalyzerError> {
        if args.len() < 2 || args.len() > 3 {
            return Ok(args);
        }

        let arg_types: Vec<DataType> = args.iter().map(|a| a.data_type.clone()).collect();
        let first_arg = &args[0];
        let first_is_array_like = matches!(&first_arg.data_type, DataType::Array(_))
            || matches!(
                &first_arg.data_type,
                DataType::UserDefined(name)
                    if name.eq_ignore_ascii_case("int2vector")
                        || name.eq_ignore_ascii_case("oidvector")
            )
            || self.is_unresolved_param(first_arg)
            || first_arg.is_null_constant();

        if !first_is_array_like {
            return Err(AnalyzerError::FunctionNotFound {
                name: func_name.to_string(),
                arg_types,
            });
        }

        let mut coerced = Vec::with_capacity(args.len());
        for (idx, arg) in args.into_iter().enumerate() {
            match idx {
                0 => coerced.push(arg),
                1 => coerced.push(self.coerce_if_needed(arg, &DataType::Int32)?),
                2 => coerced.push(self.coerce_if_needed(arg, &DataType::Boolean)?),
                _ => unreachable!("arity already validated"),
            }
        }

        Ok(coerced)
    }

    fn coerce_pg_get_indexdef_signature(
        &mut self,
        args: Vec<TypedExpr>,
    ) -> Result<Vec<TypedExpr>, AnalyzerError> {
        if args.is_empty() || args.len() > 3 {
            return Ok(args);
        }

        let mut coerced = Vec::with_capacity(args.len());
        for (idx, arg) in args.into_iter().enumerate() {
            let target = match idx {
                0 => DataType::Int64,
                1 => DataType::Int32,
                2 => DataType::Boolean,
                _ => unreachable!("arity already validated"),
            };
            coerced.push(self.coerce_if_needed(arg, &target)?);
        }
        Ok(coerced)
    }

    fn coerce_pg_get_serial_sequence_signature(
        &mut self,
        func: &Function,
        func_name: &str,
        args: Vec<TypedExpr>,
    ) -> Result<Vec<TypedExpr>, AnalyzerError> {
        if args.len() != 2 {
            return Ok(args);
        }

        let arg_types: Vec<DataType> = args.iter().map(|a| a.data_type.clone()).collect();
        let mut coerced = Vec::with_capacity(2);
        for arg in args {
            let arg_is_text_like = matches!(
                arg.data_type,
                DataType::Text | DataType::Varchar(_) | DataType::Name | DataType::Unknown
            );
            if !arg_is_text_like && !self.is_unresolved_param(&arg) && !arg.is_null_constant() {
                let display_name = match function_schema_name(func) {
                    Some(schema) => format!("{}.{}", schema, func_name.to_lowercase()),
                    None => func_name.to_lowercase(),
                };
                return Err(AnalyzerError::FunctionNotFound {
                    name: display_name,
                    arg_types,
                });
            }
            coerced.push(self.coerce_if_needed(arg, &DataType::Text)?);
        }
        Ok(coerced)
    }

    fn coerce_to_regtype_signature(
        &mut self,
        func_name: &str,
        args: Vec<TypedExpr>,
    ) -> Result<Vec<TypedExpr>, AnalyzerError> {
        if args.len() != 1 {
            return Ok(args);
        }

        let mut it = args.into_iter();
        let arg = it.next().expect("arity checked");
        let arg_types = vec![arg.data_type.clone()];
        let arg_is_text_like = matches!(
            arg.data_type,
            DataType::Text | DataType::Varchar(_) | DataType::Name | DataType::Unknown
        );
        if !arg_is_text_like && !self.is_unresolved_param(&arg) && !arg.is_null_constant() {
            return Err(AnalyzerError::FunctionNotFound {
                name: func_name.to_lowercase(),
                arg_types,
            });
        }

        Ok(vec![self.coerce_if_needed(arg, &DataType::Text)?])
    }

    fn coerce_fts_text_signature(
        &mut self,
        func_name: &str,
        args: Vec<TypedExpr>,
    ) -> Result<Vec<TypedExpr>, AnalyzerError> {
        if args.len() != 1 && args.len() != 2 {
            return Ok(args);
        }

        let arg_types: Vec<DataType> = args.iter().map(|a| a.data_type.clone()).collect();
        let mut coerced = Vec::with_capacity(args.len());
        for arg in args {
            let arg_is_text_like = matches!(
                arg.data_type,
                DataType::Text | DataType::Varchar(_) | DataType::Name | DataType::Unknown
            );
            if !arg_is_text_like && !self.is_unresolved_param(&arg) && !arg.is_null_constant() {
                return Err(AnalyzerError::FunctionNotFound {
                    name: func_name.to_string(),
                    arg_types,
                });
            }
            coerced.push(self.coerce_if_needed(arg, &DataType::Text)?);
        }
        Ok(coerced)
    }

    fn coerce_embed_text_signature(
        &mut self,
        func_name: &str,
        args: Vec<TypedExpr>,
    ) -> Result<Vec<TypedExpr>, AnalyzerError> {
        if args.len() < 2 || args.len() > 3 {
            return Ok(args);
        }

        let arg_types: Vec<DataType> = args.iter().map(|a| a.data_type.clone()).collect();
        let mut coerced = Vec::with_capacity(args.len());
        for (idx, arg) in args.into_iter().enumerate() {
            match idx {
                0 | 1 => {
                    let arg_is_text_like = matches!(
                        arg.data_type,
                        DataType::Text | DataType::Varchar(_) | DataType::Name | DataType::Unknown
                    );
                    if !arg_is_text_like
                        && !self.is_unresolved_param(&arg)
                        && !arg.is_null_constant()
                    {
                        return Err(AnalyzerError::FunctionNotFound {
                            name: func_name.to_string(),
                            arg_types: arg_types.clone(),
                        });
                    }
                    coerced.push(self.coerce_if_needed(arg, &DataType::Text)?);
                }
                2 => {
                    let arg_is_text_like = matches!(
                        arg.data_type,
                        DataType::Text | DataType::Varchar(_) | DataType::Name | DataType::Unknown
                    );
                    if !arg_is_text_like
                        && !self.is_unresolved_param(&arg)
                        && !arg.is_null_constant()
                    {
                        return Err(AnalyzerError::FunctionNotFound {
                            name: func_name.to_string(),
                            arg_types: arg_types.clone(),
                        });
                    }
                    coerced.push(self.coerce_if_needed(arg, &DataType::Text)?);
                }
                _ => unreachable!("arity already validated"),
            }
        }
        Ok(coerced)
    }

    fn coerce_embedding_signature(
        &mut self,
        func_name: &str,
        args: Vec<TypedExpr>,
    ) -> Result<Vec<TypedExpr>, AnalyzerError> {
        if args.is_empty() || args.len() > 3 {
            return Ok(args);
        }

        let arg_types: Vec<DataType> = args.iter().map(|a| a.data_type.clone()).collect();
        let mut coerced = Vec::with_capacity(args.len());
        for (idx, arg) in args.into_iter().enumerate() {
            match idx {
                0 | 1 => {
                    let arg_is_text_like = matches!(
                        arg.data_type,
                        DataType::Text | DataType::Varchar(_) | DataType::Name | DataType::Unknown
                    );
                    if !arg_is_text_like
                        && !self.is_unresolved_param(&arg)
                        && !arg.is_null_constant()
                    {
                        return Err(AnalyzerError::FunctionNotFound {
                            name: func_name.to_string(),
                            arg_types: arg_types.clone(),
                        });
                    }
                    coerced.push(self.coerce_if_needed(arg, &DataType::Text)?);
                }
                2 => {
                    let arg_is_int_like = matches!(
                        arg.data_type,
                        DataType::Int32 | DataType::Int64 | DataType::Oid
                    );
                    let arg_is_string_literal =
                        matches!(&arg.kind, TypedExprKind::Constant(Value::Text(_)));
                    if !arg_is_int_like
                        && !arg_is_string_literal
                        && !self.is_unresolved_param(&arg)
                        && !arg.is_null_constant()
                    {
                        return Err(AnalyzerError::FunctionNotFound {
                            name: func_name.to_string(),
                            arg_types: arg_types.clone(),
                        });
                    }
                    coerced.push(self.coerce_if_needed(arg, &DataType::Int64)?);
                }
                _ => unreachable!("arity already validated"),
            }
        }
        Ok(coerced)
    }

    fn coerce_vec_embed_signature(
        &mut self,
        func_name: &str,
        args: Vec<TypedExpr>,
    ) -> Result<Vec<TypedExpr>, AnalyzerError> {
        if args.len() != 2 {
            return Ok(args);
        }

        let arg_types: Vec<DataType> = args.iter().map(|a| a.data_type.clone()).collect();
        let mut args = args.into_iter();
        let first = args.next().expect("arity checked");
        let second = args.next().expect("arity checked");

        let vector_target = match (&first.data_type, &second.data_type) {
            (DataType::Vector(dim), _) => DataType::Vector(*dim),
            (_, DataType::Vector(dim)) => DataType::Vector(*dim),
            _ => DataType::Vector(0),
        };
        let first = self.coerce_if_needed(first, &vector_target)?;

        let second = if matches!(second.data_type, DataType::Vector(_)) {
            self.coerce_if_needed(second, &vector_target)?
        } else {
            let second_is_text_like = matches!(
                second.data_type,
                DataType::Text | DataType::Varchar(_) | DataType::Name | DataType::Unknown
            );
            if !second_is_text_like
                && !self.is_unresolved_param(&second)
                && !second.is_null_constant()
            {
                return Err(AnalyzerError::FunctionNotFound {
                    name: func_name.to_string(),
                    arg_types,
                });
            }
            self.coerce_if_needed(second, &DataType::Text)?
        };

        Ok(vec![first, second])
    }

    /// Create a resolved scalar FunctionCall from a function name and analyzed args.
    ///
    /// Used for syntax sugar normalization (SUBSTRING -> FunctionCall, etc.).
    /// These are known builtins, so we can rely on the registry.
    pub(in crate::sql::analyzer) fn make_function_call(
        &self,
        name: &str,
        args: Vec<TypedExpr>,
    ) -> Result<TypedExpr, AnalyzerError> {
        let arg_types: Vec<DataType> = args.iter().map(|a| a.data_type.clone()).collect();

        let registry = global_registry();
        let return_type = registry
            .resolve_return_type(name, &arg_types)
            .ok_or_else(|| AnalyzerError::FunctionNotFound {
                name: name.to_string(),
                arg_types: arg_types.clone(),
            })?;
        let return_type = refine_function_return_type(name, &args, return_type)?;

        let func = ResolvedFunction {
            name: name.to_string(),
            kind: FunctionKind::Builtin,
            return_type: return_type.clone(),
        };

        Ok(TypedExpr::new(
            TypedExprKind::FunctionCall {
                func,
                args,
                order_by: vec![],
                filter: None,
            },
            return_type,
        ))
    }

    // -- Helpers: conditional expressions --
    // These produce dedicated IR variants instead of FunctionCall,
    // because they have short-circuit or comparison semantics.

    fn analyze_coalesce(&mut self, args: Vec<TypedExpr>) -> Result<TypedExpr, AnalyzerError> {
        if args.is_empty() {
            return Err(AnalyzerError::ArgumentCountMismatch {
                function: "COALESCE".to_string(),
                expected_min: 1,
                expected_max: None,
                got: 0,
            });
        }
        let refs: Vec<&TypedExpr> = args.iter().collect();
        let unified = self.unify_expr_types(&refs, "COALESCE")?;
        let args = args
            .into_iter()
            .map(|a| self.coerce_if_needed(a, &unified))
            .collect::<Result<_, _>>()?;
        Ok(TypedExpr::new(TypedExprKind::Coalesce(args), unified))
    }

    fn analyze_nullif(&mut self, args: Vec<TypedExpr>) -> Result<TypedExpr, AnalyzerError> {
        if args.len() != 2 {
            return Err(AnalyzerError::ArgumentCountMismatch {
                function: "NULLIF".to_string(),
                expected_min: 2,
                expected_max: Some(2),
                got: args.len(),
            });
        }
        let mut it = args.into_iter();
        let a = it.next().unwrap();
        let b = it.next().unwrap();
        // NULLIF(a, b) compares a and b — coerce so comparison is type-safe.
        // PG: the result type is the first-argument type as resolved by the
        // `=` operator.  Since db9 has no cross-type `=` operators, this is
        // the coerced (common) type after ensure_comparison_compatible.
        let (a, b) = self.ensure_comparison_compatible(a, b)?;
        let dt = a.data_type.clone();
        Ok(TypedExpr::new(
            TypedExprKind::NullIf(Box::new(a), Box::new(b)),
            dt,
        ))
    }

    fn analyze_greatest_least(
        &mut self,
        args: Vec<TypedExpr>,
        is_greatest: bool,
    ) -> Result<TypedExpr, AnalyzerError> {
        let name = if is_greatest { "GREATEST" } else { "LEAST" };
        if args.is_empty() {
            return Err(AnalyzerError::ArgumentCountMismatch {
                function: name.to_string(),
                expected_min: 1,
                expected_max: None,
                got: 0,
            });
        }
        let refs: Vec<&TypedExpr> = args.iter().collect();
        let unified = self.unify_expr_types(&refs, name)?;
        let args = args
            .into_iter()
            .map(|a| self.coerce_if_needed(a, &unified))
            .collect::<Result<_, _>>()?;
        Ok(TypedExpr::new(
            TypedExprKind::MinMax { args, is_greatest },
            unified,
        ))
    }

    // -- Helper: window spec --

    #[allow(clippy::type_complexity)]
    pub(super) fn analyze_window_spec(
        &mut self,
        func: &Function,
    ) -> Result<(Vec<TypedExpr>, Vec<TypedOrderByExpr>, Option<WindowFrame>), AnalyzerError> {
        let window_type = match &func.over {
            Some(w) => w,
            None => return Ok((vec![], vec![], None)),
        };

        match window_type {
            WindowType::WindowSpec(spec) => {
                let partition_by: Vec<TypedExpr> = spec
                    .partition_by
                    .iter()
                    .map(|e| self.analyze_expr(e))
                    .collect::<Result<_, _>>()?;

                let order_by = self.analyze_order_by_exprs(&spec.order_by, &[])?;

                let window_frame = match &spec.window_frame {
                    Some(frame) => Some(self.convert_window_frame(frame)?),
                    None => None,
                };

                Ok((partition_by, order_by, window_frame))
            }
            WindowType::NamedWindow(_) => Err(AnalyzerError::Unsupported(
                "named window references".to_string(),
            )),
        }
    }

    fn convert_window_frame(
        &mut self,
        frame: &ast::WindowFrame,
    ) -> Result<WindowFrame, AnalyzerError> {
        let units = match frame.units {
            ast::WindowFrameUnits::Rows => WindowFrameUnits::Rows,
            ast::WindowFrameUnits::Range => WindowFrameUnits::Range,
            ast::WindowFrameUnits::Groups => WindowFrameUnits::Groups,
        };

        let start = self.convert_window_frame_bound(&frame.start_bound)?;
        let end = match &frame.end_bound {
            Some(b) => Some(self.convert_window_frame_bound(b)?),
            None => None,
        };

        Ok(WindowFrame { units, start, end })
    }

    fn convert_window_frame_bound(
        &mut self,
        bound: &ast::WindowFrameBound,
    ) -> Result<WindowFrameBound, AnalyzerError> {
        Ok(match bound {
            ast::WindowFrameBound::CurrentRow => WindowFrameBound::CurrentRow,
            ast::WindowFrameBound::Preceding(None) => WindowFrameBound::Preceding(None),
            ast::WindowFrameBound::Preceding(Some(e)) => {
                WindowFrameBound::Preceding(Some(Box::new(self.analyze_expr(e)?)))
            }
            ast::WindowFrameBound::Following(None) => WindowFrameBound::Following(None),
            ast::WindowFrameBound::Following(Some(e)) => {
                WindowFrameBound::Following(Some(Box::new(self.analyze_expr(e)?)))
            }
        })
    }
}
