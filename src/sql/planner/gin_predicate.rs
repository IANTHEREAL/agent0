//! GIN predicate analysis: extract `GinQual` trees from `TypedExpr` filter predicates.
//!
//! Recognises the following indexable predicate patterns:
//!
//! | Pattern                                  | GinQual shape           |
//! |------------------------------------------|-------------------------|
//! | `col @@ plainto_tsquery('hello world')`  | `And[Term(h1), Term(h2)]` |
//! | `col @@ to_tsquery('a & b | c')`        | `Or[And[T(a),T(b)], T(c)]` |
//! | `col @> '{"k":"v"}'::jsonb`             | `And[Term(h1), ...]`    |
//! | `col @> ARRAY[1,2]`                      | `And[Term(h1), Term(h2)]` |
//! | `col && ARRAY[1,2]`                      | `Or[Term(h1), Term(h2)]`  |
//!
//! Expression-index matching: `to_tsvector('chinese', col) @@ ...` matches a
//! GIN expression index on `to_tsvector('chinese', col)`.

use crate::model::{DataType, IndexDef, TableSchema, Value};
use crate::sql::analyzer::types::{BinaryOp, TypedExpr, TypedExprKind};
use crate::sql::gin::{
    extract_array_gin_tokens, extract_gin_tokens, hash_tsvector_lexeme, supported_gin_index_column,
    GinColumnType, GinIndexSource,
};
use crate::sql::planner::GinQual;
use std::collections::HashSet;

/// Result of analysing a filter predicate for GIN index applicability.
#[derive(Debug)]
pub(super) struct GinPredicateMatch {
    /// The index that can accelerate this predicate.
    pub index_id: u64,
    pub index_name: String,
    /// Boolean tree of token hashes.
    pub qual: GinQual,
    /// The original predicate expression for per-row recheck.
    pub recheck_expr: TypedExpr,
}

/// Try to extract a GIN predicate match from a filter expression for a given
/// GIN index.
///
/// Returns `None` if the predicate cannot be accelerated by this index.
pub(super) fn try_extract_gin_predicate(
    schema: &TableSchema,
    index: &IndexDef,
    filter: &TypedExpr,
) -> Option<GinPredicateMatch> {
    let (source, col_type) = supported_gin_index_column(schema, index)?;

    // Collect all matching AND-conjuncts on the same index, not just the first.
    let mut quals = Vec::new();
    let mut rechecks = Vec::new();
    for conjunct in extract_conjuncts(filter) {
        if let Some(m) = try_match_single_predicate(schema, index, &source, col_type, conjunct) {
            quals.push(m.qual);
            rechecks.push(m.recheck_expr);
        }
    }

    if quals.is_empty() {
        return None;
    }

    Some(GinPredicateMatch {
        index_id: index.id,
        index_name: index.name.clone(),
        qual: merge_gin_quals(quals),
        recheck_expr: merge_recheck_exprs(rechecks),
    })
}

/// Decompose an AND-tree into a flat list of conjuncts.
fn extract_conjuncts(expr: &TypedExpr) -> Vec<&TypedExpr> {
    match &expr.kind {
        TypedExprKind::BinaryOp {
            left,
            op: BinaryOp::And,
            right,
        } => {
            let mut v = extract_conjuncts(left);
            v.extend(extract_conjuncts(right));
            v
        }
        _ => vec![expr],
    }
}

/// Try to match a single conjunct against a GIN index.
fn try_match_single_predicate(
    schema: &TableSchema,
    index: &IndexDef,
    source: &GinIndexSource,
    col_type: GinColumnType,
    expr: &TypedExpr,
) -> Option<GinPredicateMatch> {
    let TypedExprKind::BinaryOp { left, op, right } = &expr.kind else {
        return None;
    };

    match (col_type, op) {
        // ── FTS: col @@ tsquery or to_tsvector(config, col) @@ tsquery ──
        (GinColumnType::Tsvector, BinaryOp::TsMatch) => {
            if !lhs_matches_gin_source(schema, index, source, left) {
                return None;
            }
            let qual = tsquery_value_to_gin_qual(right)?;
            // Reject pure-negative quals (NOT at root with no positive anchor)
            if !gin_qual_has_positive_term(&qual) {
                return None;
            }
            Some(GinPredicateMatch {
                index_id: index.id,
                index_name: index.name.clone(),
                qual,
                recheck_expr: expr.clone(),
            })
        }

        // ── JSONB: col @> '{"k":"v"}'::jsonb ──
        (GinColumnType::Jsonb, BinaryOp::JsonContains) => {
            if !lhs_matches_gin_source(schema, index, source, left) {
                return None;
            }
            let qual = jsonb_containment_to_gin_qual(right)?;
            Some(GinPredicateMatch {
                index_id: index.id,
                index_name: index.name.clone(),
                qual,
                recheck_expr: expr.clone(),
            })
        }

        // ── ARRAY: col @> ARRAY[...] (containment = AND) ──
        // Note: the parser maps `@>` to JsonOperator::AtArrow regardless of types,
        // so the analyzer produces JsonContains even for array containment.
        (GinColumnType::Array, BinaryOp::ArrayContains | BinaryOp::JsonContains) => {
            if !lhs_matches_gin_source(schema, index, source, left) {
                return None;
            }
            let qual = array_containment_to_gin_qual(right)?;
            Some(GinPredicateMatch {
                index_id: index.id,
                index_name: index.name.clone(),
                qual,
                recheck_expr: expr.clone(),
            })
        }

        // ── ARRAY: col && ARRAY[...] (overlap = OR) ──
        (GinColumnType::Array, BinaryOp::ArrayOverlap) => {
            if !lhs_matches_gin_source(schema, index, source, left) {
                return None;
            }
            let qual = array_overlap_to_gin_qual(right)?;
            Some(GinPredicateMatch {
                index_id: index.id,
                index_name: index.name.clone(),
                qual,
                recheck_expr: expr.clone(),
            })
        }

        _ => None,
    }
}

// ── LHS matching ────────────────────────────────────────────────

/// Check whether the LHS of a GIN predicate matches the indexed column/expression.
fn lhs_matches_gin_source(
    schema: &TableSchema,
    index: &IndexDef,
    source: &GinIndexSource,
    lhs: &TypedExpr,
) -> bool {
    match source {
        GinIndexSource::Column(col_idx) => {
            // Simple column match: LHS is a ColumnRef to the indexed column.
            if let TypedExprKind::ColumnRef { column_name, .. } = &lhs.kind {
                if let Some(idx) = schema.column_index(column_name) {
                    return idx == *col_idx;
                }
            }
            false
        }
        GinIndexSource::Expression => {
            // Expression index match: the index has `to_tsvector('config', col)`.
            // LHS must be a FunctionCall with matching shape.
            if !index.expressions.is_empty() {
                let index_expr = &index.expressions[0];
                if index_expr.contains("to_tsvector") {
                    return lhs_matches_tsvector_expression(lhs, index_expr);
                }
            }
            false
        }
    }
}

/// Check if a TypedExpr function call matches a tsvector expression index string.
///
/// E.g. the index expression `to_tsvector('chinese', col)` should match a LHS
/// like `FunctionCall { func: "to_tsvector", args: [Const("chinese"), ColumnRef("col")] }`.
fn lhs_matches_tsvector_expression(lhs: &TypedExpr, index_expr: &str) -> bool {
    let TypedExprKind::FunctionCall { func, args, .. } = &lhs.kind else {
        return false;
    };
    if !func.name.eq_ignore_ascii_case("to_tsvector") || args.len() != 2 {
        return false;
    }

    // Canonicalise the LHS as SQL and compare against the stored expression.
    let lhs_sql = crate::sql::planner::scan_type::typed_expr_to_canonical_sql(lhs);
    let lhs_norm = crate::sql::planner::scan_type::normalize_expr_string(lhs_sql);

    // Parse and normalise the index expression for comparison.
    let Some(index_ast) = crate::sql::planner::scan_type::parse_predicate_expr(index_expr) else {
        return false;
    };
    let idx_norm = crate::sql::planner::scan_type::normalize_expr_for_match(&index_ast);

    lhs_norm == idx_norm
}

// ── tsquery → GinQual conversion ────────────────────────────────

/// Convert a tsquery Value (RHS of `@@`) into a `GinQual` tree.
///
/// The tsquery string is in our internal format: `'term1' & 'term2' | 'term3'`
/// with `!` for NOT, `(` `)` for grouping.
fn tsquery_value_to_gin_qual(rhs: &TypedExpr) -> Option<GinQual> {
    // The RHS may be:
    // 1. A Constant(Tsquery(s)) — from plainto_tsquery/to_tsquery evaluated at plan time
    // 2. A FunctionCall to plainto_tsquery/to_tsquery — evaluate the constant arg
    let tsquery_str = match &rhs.kind {
        TypedExprKind::Constant(Value::Tsquery(s)) => s.clone(),
        TypedExprKind::Constant(Value::Text(s)) => s.clone(),
        TypedExprKind::FunctionCall { func, args, .. } => {
            // Try to evaluate the function at plan time if args are all constants
            let func_name = func.name.to_lowercase();
            if matches!(
                func_name.as_str(),
                "plainto_tsquery" | "to_tsquery" | "phraseto_tsquery" | "websearch_to_tsquery"
            ) {
                evaluate_tsquery_function(&func_name, args)?
            } else {
                return None;
            }
        }
        _ => return None,
    };

    if tsquery_str.is_empty() {
        return None;
    }

    tsquery_string_to_gin_qual(&tsquery_str)
}

/// Evaluate a tsquery-producing function at plan time when all args are constants.
fn evaluate_tsquery_function(func_name: &str, args: &[TypedExpr]) -> Option<String> {
    let const_args: Vec<Value> = args
        .iter()
        .map(|a| match &a.kind {
            TypedExprKind::Constant(v) => Some(v.clone()),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;

    let result = match func_name {
        "plainto_tsquery" => crate::sql::fts::plainto_tsquery(const_args).ok()?,
        "to_tsquery" => crate::sql::fts::to_tsquery(const_args).ok()?,
        "phraseto_tsquery" => crate::sql::fts::phraseto_tsquery(const_args).ok()?,
        "websearch_to_tsquery" => crate::sql::fts::websearch_to_tsquery(const_args).ok()?,
        _ => return None,
    };

    match result {
        Value::Tsquery(s) => Some(s),
        _ => None,
    }
}

/// Parse a tsquery string into a `GinQual` tree.
///
/// Uses the shared `tokenize_tsquery()` from `fts.rs` to avoid duplicate
/// parsing logic.  Phrase operators (`<->` / `<N>`) are treated as AND for
/// GIN candidate filtering — GIN only stores token hashes, not positions,
/// so phrase semantics are enforced during per-row recheck.
fn tsquery_string_to_gin_qual(tsquery: &str) -> Option<GinQual> {
    let tokens = crate::sql::fts::tokenize_tsquery(tsquery).ok()?;
    if tokens.is_empty() {
        return None;
    }
    let mut parser = TsQueryGinParser::new(&tokens);
    let qual = parser.parse_or()?;
    if parser.pos != parser.tokens.len() {
        return None;
    }
    Some(simplify_gin_qual(qual))
}

use crate::sql::fts::TsQueryToken;
struct TsQueryGinParser<'a> {
    tokens: &'a [TsQueryToken],
    pos: usize,
}
impl<'a> TsQueryGinParser<'a> {
    fn new(tokens: &'a [TsQueryToken]) -> Self {
        Self { tokens, pos: 0 }
    }
    fn parse_or(&mut self) -> Option<GinQual> {
        let mut left = self.parse_and()?;
        let mut or_children = Vec::new();
        while self.consume_if(|t| matches!(t, TsQueryToken::Or)) {
            or_children.push(left);
            left = self.parse_and()?;
        }
        if or_children.is_empty() {
            Some(left)
        } else {
            or_children.push(left);
            Some(GinQual::Or(or_children))
        }
    }
    fn parse_and(&mut self) -> Option<GinQual> {
        let mut left = self.parse_phrase()?;
        let mut and_children = Vec::new();
        while self.consume_if(|t| matches!(t, TsQueryToken::And)) {
            and_children.push(left);
            left = self.parse_phrase()?;
        }
        if and_children.is_empty() {
            Some(left)
        } else {
            and_children.push(left);
            Some(GinQual::And(and_children))
        }
    }

    /// Phrase operators (<-> / <N>) are usually treated as AND for GIN candidate
    /// filtering. GIN only stores token hashes, not positions — recheck enforces
    /// phrase semantics.
    ///
    /// If any phrase operand is negated, we must not lower to `And(..., Not(...))`
    /// because runtime set subtraction can introduce false negatives. In that case
    /// we conservatively anchor on the positive side only and let recheck enforce
    /// the full phrase semantics.
    fn parse_phrase(&mut self) -> Option<GinQual> {
        let mut operands = vec![self.parse_unary()?];
        while self.consume_if(|t| matches!(t, TsQueryToken::FollowedBy(_))) {
            operands.push(self.parse_unary()?);
        }

        if operands.len() == 1 {
            return operands.pop();
        }

        if operands.iter().any(gin_qual_contains_negation) {
            // Phrase with negation: use only a positive anchor term (superset).
            return operands.into_iter().find_map(|operand| {
                if gin_qual_contains_negation(&operand) {
                    None
                } else {
                    gin_qual_first_positive_term(&operand)
                }
            });
        }

        Some(GinQual::And(operands))
    }
    fn parse_unary(&mut self) -> Option<GinQual> {
        if self.consume_if(|t| matches!(t, TsQueryToken::Not)) {
            let inner = self.parse_unary()?;
            Some(GinQual::Not(Box::new(inner)))
        } else {
            self.parse_primary()
        }
    }
    fn parse_primary(&mut self) -> Option<GinQual> {
        if self.consume_if(|t| matches!(t, TsQueryToken::LParen)) {
            let qual = self.parse_or()?;
            if !self.consume_if(|t| matches!(t, TsQueryToken::RParen)) {
                return None;
            }
            Some(qual)
        } else if let Some(term) = self.consume_term() {
            let token_hash = hash_tsvector_lexeme(&term);
            Some(GinQual::Term { token_hash })
        } else {
            None
        }
    }

    fn consume_if(&mut self, f: impl FnOnce(&TsQueryToken) -> bool) -> bool {
        if let Some(tok) = self.tokens.get(self.pos) {
            if f(tok) {
                self.pos += 1;
                return true;
            }
        }
        false
    }
    fn consume_term(&mut self) -> Option<String> {
        if let Some(TsQueryToken::Term(s)) = self.tokens.get(self.pos) {
            self.pos += 1;
            Some(s.clone())
        } else {
            None
        }
    }
}

fn gin_qual_contains_negation(qual: &GinQual) -> bool {
    match qual {
        GinQual::Term { .. } => false,
        GinQual::And(children) | GinQual::Or(children) => {
            children.iter().any(gin_qual_contains_negation)
        }
        GinQual::Not(_) => true,
    }
}

fn simplify_gin_qual(qual: GinQual) -> GinQual {
    match qual {
        GinQual::Term { token_hash } => GinQual::Term { token_hash },
        GinQual::And(children) => {
            let mut flattened = Vec::new();
            for child in children.into_iter().map(simplify_gin_qual) {
                match child {
                    GinQual::And(grandchildren) => flattened.extend(grandchildren),
                    other => flattened.push(other),
                }
            }
            GinQual::And(flattened)
        }
        GinQual::Or(children) => {
            let mut flattened = Vec::new();
            for child in children.into_iter().map(simplify_gin_qual) {
                match child {
                    GinQual::Or(grandchildren) => flattened.extend(grandchildren),
                    other => flattened.push(other),
                }
            }
            GinQual::Or(flattened)
        }
        GinQual::Not(inner) => match simplify_gin_qual(*inner) {
            GinQual::Not(grandchild) => simplify_gin_qual(*grandchild),
            simplified_inner => GinQual::Not(Box::new(simplified_inner)),
        },
    }
}

fn merge_gin_quals(mut quals: Vec<GinQual>) -> GinQual {
    if quals.len() == 1 {
        return quals.pop().expect("single gin qual");
    }
    simplify_gin_qual(GinQual::And(quals))
}

fn merge_recheck_exprs(mut exprs: Vec<TypedExpr>) -> TypedExpr {
    if exprs.len() == 1 {
        return exprs.pop().expect("single recheck expr");
    }

    let mut iter = exprs.into_iter();
    let mut merged = iter.next().expect("at least one recheck expr");
    for expr in iter {
        merged = TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(merged),
                op: BinaryOp::And,
                right: Box::new(expr),
            },
            data_type: DataType::Boolean,
        };
    }
    merged
}

fn gin_qual_first_positive_term(qual: &GinQual) -> Option<GinQual> {
    match qual {
        GinQual::Term { token_hash } => Some(GinQual::Term {
            token_hash: *token_hash,
        }),
        GinQual::And(children) | GinQual::Or(children) => {
            children.iter().find_map(gin_qual_first_positive_term)
        }
        GinQual::Not(_) => None,
    }
}

// ── JSONB @> → GinQual ─────────────────────────────────────────

/// Convert a JSONB containment RHS to a GinQual (AND of all token hashes).
fn jsonb_containment_to_gin_qual(rhs: &TypedExpr) -> Option<GinQual> {
    let json_str = match &rhs.kind {
        TypedExprKind::Constant(Value::Jsonb(s)) => s,
        TypedExprKind::Constant(Value::Json(s)) => s,
        TypedExprKind::Constant(Value::Text(s)) => s,
        _ => return None,
    };

    let json: serde_json::Value = serde_json::from_str(json_str).ok()?;
    let tokens = extract_gin_tokens(&json);
    let mut key_value_hashes = tokens.key_values;
    let mut key_exists_hashes = tokens.key_exists;
    key_value_hashes.sort_unstable();
    key_value_hashes.dedup();
    key_exists_hashes.sort_unstable();
    key_exists_hashes.dedup();

    let mut seen = HashSet::with_capacity(key_value_hashes.len() + key_exists_hashes.len());
    let mut hashes = Vec::with_capacity(key_value_hashes.len() + key_exists_hashes.len());
    for hash in key_value_hashes
        .into_iter()
        .chain(key_exists_hashes.into_iter())
    {
        if seen.insert(hash) {
            hashes.push(hash);
        }
    }

    if hashes.is_empty() {
        return None;
    }

    let terms: Vec<GinQual> = hashes
        .into_iter()
        .map(|h| GinQual::Term { token_hash: h })
        .collect();

    if terms.len() == 1 {
        Some(terms.into_iter().next().unwrap())
    } else {
        Some(GinQual::And(terms))
    }
}

// ── ARRAY @> → GinQual ─────────────────────────────────────────

/// Convert an ARRAY containment RHS to a GinQual (AND of all element hashes).
fn array_containment_to_gin_qual(rhs: &TypedExpr) -> Option<GinQual> {
    let arr = extract_constant_array(rhs)?;
    let hashes = extract_array_gin_tokens(&arr);

    if hashes.is_empty() {
        return None;
    }

    let terms: Vec<GinQual> = hashes
        .into_iter()
        .map(|h| GinQual::Term { token_hash: h })
        .collect();

    if terms.len() == 1 {
        Some(terms.into_iter().next().unwrap())
    } else {
        Some(GinQual::And(terms))
    }
}

/// Convert an ARRAY overlap RHS to a GinQual (OR of all element hashes).
fn array_overlap_to_gin_qual(rhs: &TypedExpr) -> Option<GinQual> {
    let arr = extract_constant_array(rhs)?;
    let hashes = extract_array_gin_tokens(&arr);

    if hashes.is_empty() {
        return None;
    }

    let terms: Vec<GinQual> = hashes
        .into_iter()
        .map(|h| GinQual::Term { token_hash: h })
        .collect();

    if terms.len() == 1 {
        Some(terms.into_iter().next().unwrap())
    } else {
        Some(GinQual::Or(terms))
    }
}

/// Extract a constant array from a TypedExpr.
///
/// Handles both `Constant(Value::Array(...))` (pre-folded) and
/// `ArrayLiteral(Vec<TypedExpr>)` where each element is a constant.
fn extract_constant_array(expr: &TypedExpr) -> Option<Vec<Value>> {
    match &expr.kind {
        TypedExprKind::Constant(Value::Array(arr)) => Some(arr.clone()),
        TypedExprKind::ArrayLiteral(elems) => {
            let mut values = Vec::with_capacity(elems.len());
            for elem in elems {
                match &elem.kind {
                    TypedExprKind::Constant(v) => values.push(v.clone()),
                    // Cast-wrapped constant: e.g. CAST('a' AS text)
                    TypedExprKind::Cast { expr: inner, .. } => {
                        if let TypedExprKind::Constant(v) = &inner.kind {
                            values.push(v.clone());
                        } else {
                            return None;
                        }
                    }
                    _ => return None, // non-constant element, bail
                }
            }
            if values.is_empty() {
                None
            } else {
                Some(values)
            }
        }
        _ => None,
    }
}

// ── GinQual utilities ───────────────────────────────────────────

/// Check whether a `GinQual` tree has at least one positive `Term`.
///
/// Pure negation (e.g. `NOT foo`) cannot be evaluated via GIN alone because
/// it requires the universe of all PKs as the starting set.
pub(super) fn gin_qual_has_positive_term(qual: &GinQual) -> bool {
    match qual {
        GinQual::Term { .. } => true,
        GinQual::And(children) | GinQual::Or(children) => {
            children.iter().any(gin_qual_has_positive_term)
        }
        GinQual::Not(_) => false,
    }
}

/// Count the number of distinct terms in a `GinQual` tree (for cost estimation).
pub(super) fn gin_qual_term_count(qual: &GinQual) -> usize {
    match qual {
        GinQual::Term { .. } => 1,
        GinQual::And(children) | GinQual::Or(children) => {
            children.iter().map(gin_qual_term_count).sum()
        }
        GinQual::Not(child) => gin_qual_term_count(child),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::model::DataType;
    #[test]
    fn test_tsquery_string_to_gin_qual_single_term() {
        let qual = tsquery_string_to_gin_qual("'hello'").unwrap();
        assert!(matches!(qual, GinQual::Term { .. }));
    }

    #[test]
    fn test_tsquery_string_to_gin_qual_and() {
        let qual = tsquery_string_to_gin_qual("'hello' & 'world'").unwrap();
        match qual {
            GinQual::And(children) => assert_eq!(children.len(), 2),
            _ => panic!("expected And, got {:?}", qual),
        }
    }

    #[test]
    fn test_tsquery_string_to_gin_qual_or() {
        let qual = tsquery_string_to_gin_qual("'cat' | 'dog'").unwrap();
        match qual {
            GinQual::Or(children) => assert_eq!(children.len(), 2),
            _ => panic!("expected Or, got {:?}", qual),
        }
    }

    #[test]
    fn test_tsquery_string_to_gin_qual_not() {
        let qual = tsquery_string_to_gin_qual("'hello' & !'world'").unwrap();
        match qual {
            GinQual::And(children) => {
                assert_eq!(children.len(), 2);
                assert!(matches!(children[0], GinQual::Term { .. }));
                assert!(matches!(children[1], GinQual::Not(_)));
            }
            _ => panic!("expected And, got {:?}", qual),
        }
    }

    #[test]
    fn test_tsquery_string_to_gin_qual_double_not_simplified_in_and() {
        let qual = tsquery_string_to_gin_qual("'hello' & !!'world'").unwrap();
        match qual {
            GinQual::And(children) => {
                assert_eq!(children.len(), 2);
                assert!(matches!(children[0], GinQual::Term { .. }));
                assert!(matches!(children[1], GinQual::Term { .. }));
            }
            _ => panic!("expected And, got {:?}", qual),
        }
    }

    #[test]
    fn test_tsquery_string_to_gin_qual_root_double_not_simplified() {
        let qual = tsquery_string_to_gin_qual("!!'world'").unwrap();
        assert!(matches!(qual, GinQual::Term { .. }));
    }

    #[test]
    fn test_tsquery_phrase_to_gin_qual() {
        let qual = tsquery_string_to_gin_qual("'hello' <-> 'world'").unwrap();
        match qual {
            GinQual::And(children) => assert_eq!(children.len(), 2),
            _ => panic!("expected And, got {:?}", qual),
        }
    }

    #[test]
    fn test_tsquery_phrase_with_negated_left_uses_positive_anchor() {
        let qual = tsquery_string_to_gin_qual("!'cat' <-> 'dog'").unwrap();
        let expected = hash_tsvector_lexeme("dog");
        match qual {
            GinQual::Term { token_hash } => assert_eq!(token_hash, expected),
            _ => panic!("expected Term, got {:?}", qual),
        }
    }

    #[test]
    fn test_tsquery_phrase_with_negated_right_uses_positive_anchor() {
        let qual = tsquery_string_to_gin_qual("'cat' <-> !'dog'").unwrap();
        let expected = hash_tsvector_lexeme("cat");
        match qual {
            GinQual::Term { token_hash } => assert_eq!(token_hash, expected),
            _ => panic!("expected Term, got {:?}", qual),
        }
    }

    #[test]
    fn test_tsquery_string_to_gin_qual_complex() {
        let qual = tsquery_string_to_gin_qual("('cat' | 'dog') & 'pet'").unwrap();
        match qual {
            GinQual::And(children) => {
                assert_eq!(children.len(), 2);
                assert!(matches!(children[0], GinQual::Or(_)));
                assert!(matches!(children[1], GinQual::Term { .. }));
            }
            _ => panic!("expected And, got {:?}", qual),
        }
    }

    #[test]
    fn test_tsquery_string_empty_returns_none() {
        assert!(tsquery_string_to_gin_qual("").is_none());
    }

    #[test]
    fn test_simplify_gin_qual_double_not_term() {
        let qual = GinQual::Not(Box::new(GinQual::Not(Box::new(GinQual::Term {
            token_hash: 1,
        }))));
        let simplified = simplify_gin_qual(qual);
        match simplified {
            GinQual::Term { token_hash } => assert_eq!(token_hash, 1),
            other => panic!("expected Term, got {:?}", other),
        }
    }

    #[test]
    fn test_simplify_gin_qual_and_with_double_not() {
        let qual = GinQual::And(vec![
            GinQual::Term { token_hash: 1 },
            GinQual::Not(Box::new(GinQual::Not(Box::new(GinQual::Term {
                token_hash: 2,
            })))),
        ]);
        let simplified = simplify_gin_qual(qual);
        match simplified {
            GinQual::And(children) => {
                assert_eq!(children.len(), 2);
                assert!(matches!(children[0], GinQual::Term { token_hash: 1 }));
                assert!(matches!(children[1], GinQual::Term { token_hash: 2 }));
            }
            other => panic!("expected And, got {:?}", other),
        }
    }

    #[test]
    fn test_gin_qual_has_positive_term_pure_not() {
        let qual = GinQual::Not(Box::new(GinQual::Term { token_hash: 1 }));
        assert!(!gin_qual_has_positive_term(&qual));
    }

    #[test]
    fn test_gin_qual_has_positive_term_and_with_not() {
        let qual = GinQual::And(vec![
            GinQual::Term { token_hash: 1 },
            GinQual::Not(Box::new(GinQual::Term { token_hash: 2 })),
        ]);
        assert!(gin_qual_has_positive_term(&qual));
    }

    #[test]
    fn test_gin_qual_term_count() {
        let qual = GinQual::And(vec![
            GinQual::Term { token_hash: 1 },
            GinQual::Or(vec![
                GinQual::Term { token_hash: 2 },
                GinQual::Term { token_hash: 3 },
            ]),
            GinQual::Not(Box::new(GinQual::Term { token_hash: 4 })),
        ]);
        assert_eq!(gin_qual_term_count(&qual), 4);
    }

    #[test]
    fn test_jsonb_containment_gin_qual() {
        let expr = TypedExpr {
            kind: TypedExprKind::Constant(Value::Jsonb(r#"{"key":"val"}"#.to_string())),
            data_type: DataType::Jsonb,
        };
        let qual = jsonb_containment_to_gin_qual(&expr);
        assert!(qual.is_some());
        // Should produce AND of key-exists + key-value tokens
        match qual.unwrap() {
            GinQual::And(children) => assert!(children.len() >= 2),
            GinQual::Term { .. } => {} // single token is fine too
            other => panic!("unexpected {:?}", other),
        }
    }

    #[test]
    fn test_jsonb_containment_orders_key_value_before_key_exists() {
        let expr = TypedExpr {
            kind: TypedExprKind::Constant(Value::Jsonb(r#"{"key":"val"}"#.to_string())),
            data_type: DataType::Jsonb,
        };
        let qual = jsonb_containment_to_gin_qual(&expr).expect("jsonb gin qual");
        let tokens = extract_gin_tokens(
            &serde_json::from_str::<serde_json::Value>(r#"{"key":"val"}"#).expect("valid json"),
        );
        let mut key_value_hashes = tokens.key_values;
        key_value_hashes.sort_unstable();
        key_value_hashes.dedup();

        match qual {
            GinQual::And(children) => {
                let first = children.first().expect("at least one gin term");
                match first {
                    GinQual::Term { token_hash } => {
                        assert!(
                            key_value_hashes.contains(token_hash),
                            "expected first hash {token_hash} to be a key-value token"
                        );
                    }
                    other => panic!("expected first child to be Term, got {other:?}"),
                }
            }
            other => panic!("expected And, got {other:?}"),
        }
    }

    #[test]
    fn test_array_containment_gin_qual() {
        let expr = TypedExpr {
            kind: TypedExprKind::Constant(Value::Array(vec![Value::Int32(1), Value::Int32(2)])),
            data_type: DataType::Array(Box::new(DataType::Int32)),
        };
        let qual = array_containment_to_gin_qual(&expr);
        assert!(qual.is_some());
        match qual.unwrap() {
            GinQual::And(children) => assert_eq!(children.len(), 2),
            _ => panic!("expected And"),
        }
    }

    #[test]
    fn test_array_overlap_gin_qual() {
        let expr = TypedExpr {
            kind: TypedExprKind::Constant(Value::Array(vec![Value::Int32(1), Value::Int32(2)])),
            data_type: DataType::Array(Box::new(DataType::Int32)),
        };
        let qual = array_overlap_to_gin_qual(&expr);
        assert!(qual.is_some());
        match qual.unwrap() {
            GinQual::Or(children) => assert_eq!(children.len(), 2),
            _ => panic!("expected Or"),
        }
    }

    #[test]
    fn test_hash_consistency_with_gin_tokens() {
        // Verify that hash_tsvector_lexeme used in GinQual matches
        // the same function used during GIN index token extraction.
        let word = "hello";
        let planner_hash = hash_tsvector_lexeme(word);

        // extract_tsvector_gin_tokens produces the same hashes
        let tsvector = "'hello':1A";
        let storage_hashes = crate::sql::gin::extract_tsvector_gin_tokens(tsvector);
        assert!(storage_hashes.contains(&planner_hash));
    }
}
