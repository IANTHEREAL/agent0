//! Function signature registry

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::model::DataType;

mod aggregate_window;
mod embedding;
pub(crate) mod http;
mod json;
mod math;
mod misc;
mod string;
mod system;
mod temporal;

#[derive(Clone)]
pub enum ReturnType {
    Fixed(DataType),
    SameAsArg(usize),
    FirstNonNull,
    Custom(fn(&[DataType]) -> DataType),
}

#[derive(Clone)]
pub struct FunctionSignature {
    pub min_args: usize,
    pub max_args: Option<usize>,
    pub return_type: ReturnType,
    pub is_aggregate: bool,
    pub is_window: bool,
    /// Expected argument types, used for parameter coercion in PREPARE.
    /// Empty means no explicit arg type info (coercion handled elsewhere or skipped).
    pub arg_types: Vec<DataType>,
}

impl FunctionSignature {
    pub fn fixed(return_type: DataType) -> Self {
        Self {
            min_args: 0,
            max_args: None,
            return_type: ReturnType::Fixed(return_type),
            is_aggregate: false,
            is_window: false,
            arg_types: Vec::new(),
        }
    }

    pub fn with_args(mut self, min: usize, max: Option<usize>) -> Self {
        self.min_args = min;
        self.max_args = max;
        self
    }

    pub fn aggregate(mut self) -> Self {
        self.is_aggregate = true;
        self
    }

    pub fn window(mut self) -> Self {
        self.is_window = true;
        self
    }

    pub fn same_as_arg(arg_index: usize) -> Self {
        Self {
            min_args: arg_index + 1,
            max_args: None,
            return_type: ReturnType::SameAsArg(arg_index),
            is_aggregate: false,
            is_window: false,
            arg_types: Vec::new(),
        }
    }

    pub fn custom(f: fn(&[DataType]) -> DataType) -> Self {
        Self {
            min_args: 0,
            max_args: None,
            return_type: ReturnType::Custom(f),
            is_aggregate: false,
            is_window: false,
            arg_types: Vec::new(),
        }
    }

    pub fn with_arg_types(mut self, types: Vec<DataType>) -> Self {
        self.arg_types = types;
        self
    }
}

pub struct FunctionRegistry {
    // Each function name maps to a list of overloads. Most builtins
    // register exactly one overload and behave the same as before;
    // functions that match PostgreSQL's multi-overload signature
    // (SIGN, ABS's 6 numeric variants, etc.) register one entry per
    // concrete (arg_types, return_type) pair. pg_proc emits one row
    // per overload — matching PG's catalog exactly — and analyzer
    // overload resolution picks the best-matching signature for a
    // given call.
    functions: HashMap<String, Vec<FunctionSignature>>,
}

impl FunctionRegistry {
    pub fn new() -> Self {
        Self {
            functions: HashMap::new(),
        }
    }

    /// Register a function overload.
    ///
    /// Semantics: keyed on `(name, sig.arg_types)`. If a prior overload
    /// with the same declared argument-type list exists, this call
    /// **replaces** it — matching the old single-signature registry's
    /// insert-based "last write wins" behavior so legacy duplicate
    /// registrations (e.g. `HAS_TABLE_PRIVILEGE` in both `system.rs`
    /// and `misc.rs`) do not silently become duplicate pg_proc rows.
    /// If the arg-type list differs from every existing overload, the
    /// new sig is **appended** — this is how a function gets multiple
    /// distinct overloads (SIGN registers `[dp]` and `[numeric]` to
    /// match PG's pg_proc layout).
    ///
    /// Resolution across distinct overloads is exact-match > implicit
    /// coercion > polymorphic, with first-registered as the tie-breaker
    /// (see `select_overload`).
    pub fn register(&mut self, name: &str, sig: FunctionSignature) {
        let entries = self.functions.entry(name.to_uppercase()).or_default();
        if let Some(existing) = entries.iter_mut().find(|e| e.arg_types == sig.arg_types) {
            *existing = sig;
        } else {
            entries.push(sig);
        }
    }

    /// Returns the *primary* (first-registered) overload for a name.
    /// Callers should only rely on fields that are invariant across
    /// overloads of the same function: `min_args`, `max_args`,
    /// `is_aggregate`, `is_window`. For return-type resolution use
    /// [`resolve_return_type`] so overload selection is honored.
    pub fn get(&self, name: &str) -> Option<&FunctionSignature> {
        self.functions.get(&name.to_uppercase())?.first()
    }

    /// All (name, overload) pairs. pg_proc uses this to emit one row
    /// per overload, so a 2-overload SIGN shows up as two rows in
    /// `pg_catalog.pg_proc`, matching PG.
    pub fn iter_overloads(&self) -> impl Iterator<Item = (&str, &FunctionSignature)> {
        self.functions
            .iter()
            .flat_map(|(name, sigs)| sigs.iter().map(move |s| (name.as_str(), s)))
    }

    pub fn resolve_return_type(&self, name: &str, arg_types: &[DataType]) -> Option<DataType> {
        let sigs = self.functions.get(&name.to_uppercase())?;
        let sig = select_overload(sigs, arg_types)?;
        Some(match &sig.return_type {
            ReturnType::Fixed(dt) => dt.clone(),
            ReturnType::SameAsArg(idx) => arg_types.get(*idx).cloned()?,
            ReturnType::FirstNonNull => arg_types.first().cloned()?,
            ReturnType::Custom(f) => f(arg_types),
        })
    }

    /// Return the best-matching overload for a call with the given
    /// argument types, using the same ranking as `resolve_return_type`
    /// (exact match > implicit coercion > polymorphic fallback). Used
    /// by the analyzer's argument-coercion / PREPARE parameter
    /// inference so it can pick per-call overload arg_types rather
    /// than always reaching for the first-registered signature —
    /// which is load-bearing for multi-overload functions like SIGN
    /// (e.g. `PREPARE q(numeric) AS SELECT sign($1)` must pick the
    /// numeric overload, not the primary dp one).
    pub fn resolve_overload(
        &self,
        name: &str,
        arg_types: &[DataType],
    ) -> Option<&FunctionSignature> {
        let sigs = self.functions.get(&name.to_uppercase())?;
        select_overload(sigs, arg_types)
    }

    pub fn arity_supported(&self, name: &str, arg_count: usize) -> bool {
        self.functions
            .get(&name.to_uppercase())
            .is_some_and(|sigs| sigs.iter().any(|sig| sig_accepts_arity(sig, arg_count)))
    }
}

/// Pick the best-matching overload for the given argument types,
/// mirroring PostgreSQL's implicit numeric-cast rules just enough to
/// resolve the overloads we register today. Ranking (descending):
///
/// 1. Exact type match on every argument.
/// 2. Coercion allowed (int → dp, int → numeric, int/dp for numeric
///    overloads). Earlier registrations win ties, so registration
///    order should put the "preferred" overload first (e.g. SIGN
///    registers dp before numeric so SIGN(int) resolves to dp, as
///    PG does).
/// 3. A polymorphic / empty-arg_types overload (SameAsArg, Custom,
///    variadic-text functions with no declared types).
fn select_overload<'a>(
    sigs: &'a [FunctionSignature],
    arg_types: &[DataType],
) -> Option<&'a FunctionSignature> {
    if sigs.len() == 1 {
        // Fast path: single overload, behavior identical to the
        // pre-refactor single-sig registry. Some builtins still have
        // incomplete registry overload coverage even though runtime supports
        // the PG surface (e.g. length(bytea)); strict validation belongs in
        // per-function analyzer rules until those overloads are registered.
        return sigs.first();
    }
    let mut best: Option<(u32, &FunctionSignature)> = None;
    for sig in sigs {
        let Some(score) = overload_match_score(sig, arg_types) else {
            continue;
        };
        match best {
            Some((best_score, _)) if best_score >= score => {}
            _ => best = Some((score, sig)),
        }
    }
    best.map(|(_, s)| s)
}

/// `None` means the overload doesn't accept these argument types at
/// all; `Some(n)` is a match with score `n` (higher = better).
///
/// Scoring has two tiers so exact matches win over implicit-coercion
/// matches (PG's overload resolution rank 3a beats 3b). Ties within a
/// tier resolve to the first-registered overload, so registration
/// order should put the preferred overload first (e.g. SIGN registers
/// dp before numeric, so SIGN(int) picks dp — matching PG's preferred
/// numeric-category type).
fn overload_match_score(sig: &FunctionSignature, actual: &[DataType]) -> Option<u32> {
    if !sig_accepts_arity(sig, actual.len()) {
        return None;
    }
    if sig.arg_types.is_empty() {
        // Polymorphic / unrestricted — last-resort fallback.
        return Some(1);
    }
    if sig.arg_types.len() != actual.len() {
        return None;
    }
    let mut score: u32 = 100;
    for (decl, act) in sig.arg_types.iter().zip(actual) {
        if decl == act {
            score += 10;
        } else if matches!(act, DataType::Unknown) {
            // PostgreSQL can coerce unknown string literals to any candidate
            // function argument type. Keep this below exact matches so concrete
            // overloads still win, and let registration order break ties.
        } else if super::coercion::is_implicitly_coercible(act, decl) {
            // Implicit cast matches but doesn't earn the +10 bonus, so
            // exact-match overloads outrank coerced-match overloads.
        } else {
            return None;
        }
    }
    Some(score)
}

fn sig_accepts_arity(sig: &FunctionSignature, arg_count: usize) -> bool {
    arg_count >= sig.min_args && sig.max_args.is_none_or(|max| arg_count <= max)
}

pub fn global_registry() -> &'static FunctionRegistry {
    static REGISTRY: OnceLock<FunctionRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut r = FunctionRegistry::new();
        register_builtin_functions(&mut r);
        r
    })
}

fn register_builtin_functions(r: &mut FunctionRegistry) {
    aggregate_window::register(r);
    embedding::register(r);
    http::register(r);
    string::register(r);
    math::register(r);
    temporal::register(r);
    json::register(r);
    system::register(r);
    misc::register(r);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Legacy callers register the same `(name, arg_types)` pair from
    /// multiple modules (e.g. `HAS_TABLE_PRIVILEGE` in both
    /// `system.rs` and `misc.rs`). Before multi-overload the map-based
    /// registry silently overwrote; after this refactor the same
    /// invariant must hold via `register()`'s dedupe — otherwise those
    /// callers would produce duplicate pg_proc rows with identical
    /// (oid, proargtypes), violating PG's catalog uniqueness.
    #[test]
    fn register_dedupes_same_arg_types() {
        let mut r = FunctionRegistry::new();
        r.register(
            "FOO",
            FunctionSignature::fixed(DataType::Int32).with_args(1, Some(1)),
        );
        r.register(
            "FOO",
            FunctionSignature::fixed(DataType::Int64).with_args(1, Some(1)),
        );
        let sigs = r.functions.get("FOO").expect("registered");
        assert_eq!(
            sigs.len(),
            1,
            "same arg_types must dedupe (last write wins)"
        );
        assert!(matches!(
            sigs[0].return_type,
            ReturnType::Fixed(DataType::Int64)
        ));
    }

    /// Distinct arg_types are genuinely distinct overloads (the whole
    /// point of multi-overload support): SIGN registers `[dp]` and
    /// `[numeric]` and both must persist so pg_proc emits two rows and
    /// overload resolution can route calls to the right return type.
    #[test]
    fn register_keeps_distinct_overloads() {
        let numeric = DataType::Numeric {
            precision: None,
            scale: None,
        };
        let mut r = FunctionRegistry::new();
        r.register(
            "SIGN",
            FunctionSignature::fixed(DataType::Float64)
                .with_args(1, Some(1))
                .with_arg_types(vec![DataType::Float64]),
        );
        r.register(
            "SIGN",
            FunctionSignature::fixed(numeric.clone())
                .with_args(1, Some(1))
                .with_arg_types(vec![numeric.clone()]),
        );
        let sigs = r.functions.get("SIGN").expect("registered");
        assert_eq!(sigs.len(), 2);
        assert_eq!(
            r.resolve_return_type("sign", &[DataType::Int32]),
            Some(DataType::Float64),
            "int coerces to dp overload (PG preferred numeric-category type)"
        );
        assert_eq!(
            r.resolve_return_type("sign", &[DataType::Float64]),
            Some(DataType::Float64),
            "dp is an exact match"
        );
        assert_eq!(
            r.resolve_return_type(
                "sign",
                &[DataType::Numeric {
                    precision: Some(10),
                    scale: Some(2),
                }]
            ),
            Some(numeric.clone()),
            "numeric input resolves to numeric overload regardless of typmod"
        );
    }
}
