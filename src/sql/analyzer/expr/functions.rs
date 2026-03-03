//! Function, CASE, and conditional expression analysis.
//!
//! Contains `analyze_case`, `analyze_function`, `analyze_coalesce`,
//! `analyze_nullif`, `analyze_greatest_least`, window spec helpers,
//! and `make_function_call`.

use sqlparser::ast::{self as ast, Expr, Function, FunctionArg, FunctionArgExpr, WindowType};

use crate::model::{DataType, Value};
use crate::sql::names::function_name_upper;
use crate::sql::types::coercion::comparison_target_type;
use crate::sql::types::mapping::sql_datatype_to_internal;
use crate::sql::types::registry::global_registry;

use crate::sql::analyzer::error::AnalyzerError;
use crate::sql::analyzer::types::*;
use crate::sql::analyzer::Analyzer;

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

impl<'a> Analyzer<'a> {
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
                            left: target.to_string().to_lowercase(),
                            right: when_expr.data_type.to_string().to_lowercase(),
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
        let mut func_name = function_name_upper(func);

        // Preserve schema prefix for schema-qualified functions (e.g., cron.schedule).
        // function_name_upper() only takes the last segment of ObjectName, stripping
        // schema qualifiers. We need the full qualified name for dispatch in classify.rs,
        // materialize.rs, and typed_eval.rs.
        if func.name.0.len() > 1 {
            let schema = func.name.0[0].value.to_lowercase();
            if schema == "cron" {
                func_name = format!("cron.{}", func_name);
            }
        }

        // Extract function arguments
        let args = self.extract_function_args(func)?;

        // Analyze argument expressions
        let analyzed_args: Vec<TypedExpr> = args
            .iter()
            .map(|a| self.analyze_expr(a))
            .collect::<Result<_, _>>()?;
        let analyzed_args = self.apply_function_arg_context(func_name.as_str(), analyzed_args)?;

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
                    arg_types: analyzed_args.iter().map(|a| a.data_type.clone()).collect(),
                });
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

        // Not in builtin registry -- check catalog for UDF
        if let Ok(Some(func_def)) = self.catalog.resolve_function(&func_name, None, &arg_types) {
            let return_type = sql_datatype_to_internal(&sqlparser::ast::DataType::Custom(
                sqlparser::ast::ObjectName(vec![ast::Ident::new(&func_def.return_type)]),
                vec![],
            ))
            .map_err(|e| {
                AnalyzerError::Unsupported(format!(
                    "UDF {}: unsupported return type '{}': {}",
                    func_name, func_def.return_type, e
                ))
            })?;

            let resolved = ResolvedFunction {
                name: func_name,
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
            "TO_REGTYPE" => self.coerce_to_regtype_signature(func_name, args),
            "TO_TSVECTOR"
            | "PLAINTO_TSQUERY"
            | "PHRASETO_TSQUERY"
            | "TO_TSQUERY"
            | "WEBSEARCH_TO_TSQUERY" => self.coerce_fts_text_signature(func_name, args),
            _ if is_two_arg_advisory_lock_function(func_name) => {
                self.coerce_advisory_lock_two_arg_signature(func_name, args)
            }
            _ => Ok(args),
        }
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
            DataType::Text | DataType::Varchar(_) | DataType::Name
        );
        if !arg_is_text_like && !self.is_unresolved_param(&arg) && !arg.is_null_constant() {
            return Err(AnalyzerError::FunctionNotFound {
                name: func_name.to_string(),
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
                DataType::Text | DataType::Varchar(_) | DataType::Name
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
        let dt = args[0].data_type.clone();
        let mut it = args.into_iter();
        let a = it.next().unwrap();
        let b = it.next().unwrap();
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
