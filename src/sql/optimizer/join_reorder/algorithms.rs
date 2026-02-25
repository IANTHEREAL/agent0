//! DPccp and greedy join reordering algorithms.
//!
//! Contains the core optimization algorithms, edge graph construction,
//! join candidate building, expression remapping, plan finalization,
//! and subset iteration helpers.

use std::collections::HashMap;

use crate::sql::analyzer::types::{JoinCondition, JoinType, TypedExpr, TypedExprKind};
use crate::sql::expr::traverse::map_children;

use super::super::logical_plan::{LogicalNode, LogicalPlan, PlanSchema};
use super::super::physical_planner::PlanningContext;
use super::super::rewrite::conjuncts_to_predicate;
use super::cost::{estimate_candidate_rows, estimate_subtree_rows};
use super::predicates::{BaseRelation, JoinEdge};

// ── DPccp DP state ──────────────────────────────────────────────

/// A candidate partial join plan tracked during DP.
#[derive(Clone)]
struct DpEntry {
    /// Bitmask of included base relations.
    set: u64,
    /// The constructed logical plan for this subset.
    plan: LogicalPlan,
    /// Estimated output rows.
    rows: usize,
    /// Cumulative cost.
    cost: f64,
    /// Column mapping: `col_map[root_col_idx] = local_col_idx` in this plan.
    col_map: HashMap<usize, usize>,
}

// ── DPccp optimizer ─────────────────────────────────────────────

pub(super) fn dpccp_optimize(
    rels: &[BaseRelation],
    edges: &[JoinEdge],
    remaining: &[TypedExpr],
    ctx: &PlanningContext,
    root_schema: &PlanSchema,
) -> LogicalPlan {
    let n = rels.len();
    let full_set = (1u64 << n) - 1;

    // Initialize DP table with single-relation entries
    let mut dp: HashMap<u64, DpEntry> = HashMap::new();
    for rel in rels {
        let mask = 1u64 << rel.id;
        let rows = estimate_subtree_rows(&rel.plan, ctx);
        let cost = rows as f64 * 0.01 + 1.0;
        let mut col_map = HashMap::new();
        for i in 0..rel.width {
            col_map.insert(rel.col_offset + i, i);
        }
        dp.insert(
            mask,
            DpEntry {
                set: mask,
                plan: rel.plan.clone(),
                rows,
                cost,
                col_map,
            },
        );
    }

    // Build adjacency set from edges
    let edge_graph = build_edge_graph(edges, n);

    // Enumerate subsets in increasing size
    for size in 2..=n {
        for subset in SubsetIter::new(full_set, size) {
            // Try all ways to split subset into (s1, s2) where s1 < s2
            // and s1, s2 are connected via at least one edge
            for s1 in SubsetIter::non_empty_subsets(subset) {
                let s2 = subset & !s1;
                if s2 == 0 || s1 >= s2 {
                    continue; // Avoid duplicates: ensure s1 < s2
                }
                // Both must be in DP table
                let (e1, e2) = match (dp.get(&s1), dp.get(&s2)) {
                    (Some(a), Some(b)) => (a.clone(), b.clone()),
                    _ => continue,
                };
                // Must be connected by at least one edge
                if !subsets_connected(s1, s2, &edge_graph) {
                    continue;
                }
                let candidate = build_join_candidate(&e1, &e2, edges, ctx, rels);
                // Update DP table if this is better
                let existing = dp.get(&subset);
                if existing.is_none() || candidate.cost < existing.unwrap().cost {
                    dp.insert(subset, candidate);
                }
            }
        }
    }

    // Build final plan
    if let Some(entry) = dp.get(&full_set) {
        finalize_plan(entry, remaining, root_schema, rels)
    } else {
        // Disconnected graph: find connected components and stitch
        stitch_disconnected_components(&dp, rels, edges, remaining, ctx, root_schema, n)
    }
}

// ── Greedy optimizer ────────────────────────────────────────────

pub(super) fn greedy_optimize(
    rels: &[BaseRelation],
    edges: &[JoinEdge],
    remaining: &[TypedExpr],
    ctx: &PlanningContext,
    root_schema: &PlanSchema,
) -> LogicalPlan {
    let n = rels.len();

    // Initialize candidates
    let mut candidates: Vec<DpEntry> = rels
        .iter()
        .map(|rel| {
            let mask = 1u64 << rel.id;
            let rows = estimate_subtree_rows(&rel.plan, ctx);
            let cost = rows as f64 * 0.01 + 1.0;
            let mut col_map = HashMap::new();
            for i in 0..rel.width {
                col_map.insert(rel.col_offset + i, i);
            }
            DpEntry {
                set: mask,
                plan: rel.plan.clone(),
                rows,
                cost,
                col_map,
            }
        })
        .collect();

    let edge_graph = build_edge_graph(edges, n);

    // Greedily merge the cheapest connected pair
    while candidates.len() > 1 {
        let mut best_cost = f64::MAX;
        let mut best_i = 0;
        let mut best_j = 1;
        let mut best_candidate: Option<DpEntry> = None;

        for i in 0..candidates.len() {
            for j in (i + 1)..candidates.len() {
                // Prefer connected pairs
                if !subsets_connected(candidates[i].set, candidates[j].set, &edge_graph) {
                    continue;
                }
                let c = build_join_candidate(&candidates[i], &candidates[j], edges, ctx, rels);
                if c.cost < best_cost {
                    best_cost = c.cost;
                    best_i = i;
                    best_j = j;
                    best_candidate = Some(c);
                }
            }
        }

        if best_candidate.is_none() {
            // No connected pairs left: stitch disconnected components with cross join.
            // Sort ascending by row count and merge smallest-first to minimize
            // intermediate result sizes (matches stitch_disconnected_components).
            candidates.sort_by_key(|c| c.rows);
            let mut result = candidates.remove(0);
            for other in candidates.drain(..) {
                result = build_cross_join_candidate(&result, &other);
            }
            candidates.push(result);
            break;
        }

        let merged = best_candidate.unwrap();
        // Remove j first (higher index), then i
        candidates.remove(best_j);
        candidates.remove(best_i);
        candidates.push(merged);
    }

    let entry = &candidates[0];
    finalize_plan(entry, remaining, root_schema, rels)
}

// ── Join candidate builder ──────────────────────────────────────

/// Build a candidate join of two DP entries.
fn build_join_candidate(
    left: &DpEntry,
    right: &DpEntry,
    edges: &[JoinEdge],
    ctx: &PlanningContext,
    rels: &[BaseRelation],
) -> DpEntry {
    let combined_set = left.set | right.set;

    // Collect applicable edges: edge.rels ⊆ combined AND touches both sides
    let mut equi_preds: Vec<TypedExpr> = Vec::new();
    let mut residual_preds: Vec<TypedExpr> = Vec::new();

    for edge in edges {
        if (edge.rels & combined_set) == edge.rels
            && (edge.rels & left.set) != 0
            && (edge.rels & right.set) != 0
        {
            if edge.is_equi {
                equi_preds.push(edge.predicate.clone());
            } else {
                residual_preds.push(edge.predicate.clone());
            }
        }
    }

    // Build col_map for the combined plan
    let left_width = left.plan.schema.columns.len();
    let mut new_col_map = HashMap::new();
    for (&orig, &local) in &left.col_map {
        new_col_map.insert(orig, local);
    }
    for (&orig, &local) in &right.col_map {
        new_col_map.insert(orig, left_width + local);
    }

    // Build join schema
    let mut combined_cols = left.plan.schema.columns.clone();
    combined_cols.extend(right.plan.schema.columns.clone());
    let join_schema = PlanSchema::from_columns(combined_cols);

    // Remap equi predicates to local indices
    let remapped_equi: Vec<TypedExpr> = equi_preds
        .iter()
        .map(|p| remap_expr(p, &new_col_map))
        .collect();

    // Build the join node
    let (join_type, condition) = if !remapped_equi.is_empty() {
        (
            JoinType::Inner,
            JoinCondition::On(conjuncts_to_predicate(remapped_equi)),
        )
    } else {
        (JoinType::Inner, JoinCondition::None)
    };

    let mut plan = LogicalPlan {
        node: LogicalNode::Join {
            left: Box::new(left.plan.clone()),
            right: Box::new(right.plan.clone()),
            join_type,
            condition: condition.clone(),
        },
        schema: join_schema.clone(),
    };

    // Wrap with residual filter if needed
    if !residual_preds.is_empty() {
        let remapped_residual: Vec<TypedExpr> = residual_preds
            .iter()
            .map(|p| remap_expr(p, &new_col_map))
            .collect();
        let pred = conjuncts_to_predicate(remapped_residual);
        plan = LogicalPlan {
            node: LogicalNode::Filter {
                predicate: pred,
                input: Box::new(plan),
            },
            schema: join_schema,
        };
    }

    // Estimate rows and cost
    let left_rows = left.rows;
    let right_rows = right.rows;
    let output_rows = estimate_candidate_rows(
        &left.plan,
        &right.plan,
        left_rows,
        right_rows,
        &equi_preds,
        ctx,
        rels,
    );
    let cost = left.cost + right.cost + output_rows as f64 * 0.01;

    DpEntry {
        set: combined_set,
        plan,
        rows: output_rows,
        cost,
        col_map: new_col_map,
    }
}

/// Build a cross join candidate (for disconnected components).
fn build_cross_join_candidate(left: &DpEntry, right: &DpEntry) -> DpEntry {
    let combined_set = left.set | right.set;
    let left_width = left.plan.schema.columns.len();

    let mut new_col_map = HashMap::new();
    for (&orig, &local) in &left.col_map {
        new_col_map.insert(orig, local);
    }
    for (&orig, &local) in &right.col_map {
        new_col_map.insert(orig, left_width + local);
    }

    let mut combined_cols = left.plan.schema.columns.clone();
    combined_cols.extend(right.plan.schema.columns.clone());
    let join_schema = PlanSchema::from_columns(combined_cols);

    let plan = LogicalPlan {
        node: LogicalNode::Join {
            left: Box::new(left.plan.clone()),
            right: Box::new(right.plan.clone()),
            join_type: JoinType::Inner,
            condition: JoinCondition::None,
        },
        schema: join_schema,
    };

    let output_rows = left.rows.saturating_mul(right.rows);
    let cost = left.cost + right.cost + output_rows as f64 * 0.01;

    DpEntry {
        set: combined_set,
        plan,
        rows: output_rows,
        cost,
        col_map: new_col_map,
    }
}

// ── Edge graph and connectivity ─────────────────────────────────

/// Build adjacency: `edge_graph[i]` is the set of relations adjacent to relation i.
fn build_edge_graph(edges: &[JoinEdge], n: usize) -> Vec<u64> {
    let mut graph = vec![0u64; n];
    for edge in edges {
        let bits: Vec<usize> = (0..n).filter(|&i| edge.rels & (1u64 << i) != 0).collect();
        if bits.len() == 2 {
            graph[bits[0]] |= 1u64 << bits[1];
            graph[bits[1]] |= 1u64 << bits[0];
        }
    }
    graph
}

/// Check if two subsets are connected by at least one edge.
fn subsets_connected(s1: u64, s2: u64, edge_graph: &[u64]) -> bool {
    for (i, edges) in edge_graph.iter().enumerate() {
        if s1 & (1u64 << i) == 0 {
            continue;
        }
        if edges & s2 != 0 {
            return true;
        }
    }
    false
}

/// Find connected components in the full relation set.
fn find_connected_components(n: usize, edge_graph: &[u64]) -> Vec<u64> {
    let mut visited = 0u64;
    let mut components = Vec::new();

    for start in 0..n {
        if visited & (1u64 << start) != 0 {
            continue;
        }
        let mut component = 0u64;
        let mut stack = vec![start];
        while let Some(node) = stack.pop() {
            if component & (1u64 << node) != 0 {
                continue;
            }
            component |= 1u64 << node;
            visited |= 1u64 << node;
            for neighbor in 0..n {
                if edge_graph[node] & (1u64 << neighbor) != 0 && component & (1u64 << neighbor) == 0
                {
                    stack.push(neighbor);
                }
            }
        }
        components.push(component);
    }

    components
}

/// Handle disconnected graph: optimize each component independently, stitch with cross joins.
fn stitch_disconnected_components(
    dp: &HashMap<u64, DpEntry>,
    rels: &[BaseRelation],
    edges: &[JoinEdge],
    remaining: &[TypedExpr],
    _ctx: &PlanningContext,
    root_schema: &PlanSchema,
    n: usize,
) -> LogicalPlan {
    let edge_graph = build_edge_graph(edges, n);
    let components = find_connected_components(n, &edge_graph);

    // For each component, get the DP entry or build from single rel
    let mut component_entries: Vec<DpEntry> = Vec::new();
    for &comp in &components {
        if let Some(entry) = dp.get(&comp) {
            component_entries.push(entry.clone());
        } else {
            // Single-relation component
            for i in 0..n {
                if comp == (1u64 << i) {
                    if let Some(entry) = dp.get(&comp) {
                        component_entries.push(entry.clone());
                    }
                }
            }
        }
    }

    // Sort by ascending rows for deterministic, efficient cross-join order
    component_entries.sort_by_key(|e| e.rows);

    // Stitch together with cross joins
    let mut result = component_entries.remove(0);
    for other in component_entries {
        result = build_cross_join_candidate(&result, &other);
    }

    finalize_plan(&result, remaining, root_schema, rels)
}

// ── Remap expressions ───────────────────────────────────────────

/// Remap a root-level expression to local indices using a column map.
fn remap_expr(expr: &TypedExpr, col_map: &HashMap<usize, usize>) -> TypedExpr {
    let kind = match &expr.kind {
        TypedExprKind::ColumnRef {
            scope_depth: 0,
            column_index,
            column_name,
        } => {
            let new_idx = col_map.get(column_index).copied().unwrap_or(*column_index);
            TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: new_idx,
                column_name: column_name.clone(),
            }
        }
        _ => map_children(expr, &mut |child| remap_expr(child, col_map)),
    };
    TypedExpr {
        kind,
        data_type: expr.data_type.clone(),
    }
}

// ── Finalization ────────────────────────────────────────────────

/// Build the final plan from a DP entry: apply remaining predicates and remap.
fn finalize_plan(
    entry: &DpEntry,
    remaining: &[TypedExpr],
    root_schema: &PlanSchema,
    rels: &[BaseRelation],
) -> LogicalPlan {
    let mut plan = entry.plan.clone();

    // Apply remaining predicates (0-rel or 3+-rel)
    if !remaining.is_empty() {
        let remapped: Vec<TypedExpr> = remaining
            .iter()
            .map(|p| remap_expr(p, &entry.col_map))
            .collect();
        let pred = conjuncts_to_predicate(remapped);
        let schema = plan.schema.clone();
        plan = LogicalPlan {
            node: LogicalNode::Filter {
                predicate: pred,
                input: Box::new(plan),
            },
            schema,
        };
    }

    // Check if columns need reordering
    let total_root_cols: usize = rels.iter().map(|r| r.width).sum();
    let needs_remap =
        (0..total_root_cols).any(|i| entry.col_map.get(&i).copied().unwrap_or(i) != i);

    if needs_remap {
        // Build a project node that reorders columns back to original order
        let mut projections = Vec::new();
        let mut output_cols = Vec::new();
        for root_idx in 0..total_root_cols {
            let local_idx = entry.col_map.get(&root_idx).copied().unwrap_or(root_idx);
            let (col_name, col_type) = &plan.schema.columns[local_idx];
            projections.push(crate::sql::analyzer::types::AnalyzedProjection {
                expr: TypedExpr {
                    kind: TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: local_idx,
                        column_name: col_name.clone(),
                    },
                    data_type: col_type.clone(),
                },
                output_name: col_name.clone(),
            });
            output_cols.push((col_name.clone(), col_type.clone()));
        }
        plan = plan.project(projections, PlanSchema::from_columns(output_cols));
    }

    // Ensure output schema matches root
    plan.schema = root_schema.clone();
    plan
}

// ── Subset iteration helpers ────────────────────────────────────

struct SubsetIter {
    universe: u64,
    target_size: usize,
    current: Option<u64>,
}

impl SubsetIter {
    fn new(universe: u64, target_size: usize) -> Self {
        // Find the first subset of the given size
        let first = first_subset_of_size(universe, target_size);
        Self {
            universe,
            target_size,
            current: first,
        }
    }

    /// Iterate all non-empty subsets of a given set.
    fn non_empty_subsets(set: u64) -> NonEmptySubsetIter {
        NonEmptySubsetIter {
            set,
            current: Some(set), // Start with the full set
            started: false,
        }
    }
}

impl Iterator for SubsetIter {
    type Item = u64;

    fn next(&mut self) -> Option<u64> {
        let result = self.current?;
        // Find next subset of the same size using Gosper's hack
        self.current = next_subset_of_size(result, self.universe, self.target_size);
        Some(result)
    }
}

struct NonEmptySubsetIter {
    set: u64,
    current: Option<u64>,
    started: bool,
}

impl Iterator for NonEmptySubsetIter {
    type Item = u64;

    fn next(&mut self) -> Option<u64> {
        if !self.started {
            self.started = true;
            // Start from (set - 1) & set, the first proper subset
            let first = (self.set.wrapping_sub(1)) & self.set;
            if first == 0 {
                return None;
            }
            self.current = Some(first);
            return self.current;
        }
        let c = self.current?;
        let next = (c.wrapping_sub(1)) & self.set;
        if next == 0 {
            self.current = None;
            return None;
        }
        self.current = Some(next);
        Some(next)
    }
}

/// Find the first subset of `universe` with exactly `target_size` bits set.
fn first_subset_of_size(universe: u64, target_size: usize) -> Option<u64> {
    if target_size == 0 {
        return Some(0);
    }
    // Collect bits in universe
    let bits: Vec<u32> = (0..64).filter(|&i| universe & (1u64 << i) != 0).collect();
    if bits.len() < target_size {
        return None;
    }
    // First subset = lowest `target_size` bits
    let mut result = 0u64;
    for bit in bits.iter().take(target_size) {
        result |= 1u64 << bit;
    }
    Some(result)
}

/// Find the next subset of `universe` with `target_size` bits, using Gosper's hack variant.
fn next_subset_of_size(current: u64, universe: u64, target_size: usize) -> Option<u64> {
    // Collect universe bits in order
    let bits: Vec<u32> = (0..64).filter(|&i| universe & (1u64 << i) != 0).collect();
    let n = bits.len();
    if target_size > n {
        return None;
    }

    // Convert current to index combination
    let mut indices: Vec<usize> = Vec::new();
    for (pos, &bit) in bits.iter().enumerate() {
        if current & (1u64 << bit) != 0 {
            indices.push(pos);
        }
    }

    // Find next combination
    if !next_combination(&mut indices, n) {
        return None;
    }

    let mut result = 0u64;
    for &idx in &indices {
        result |= 1u64 << bits[idx];
    }
    Some(result)
}

/// Advance a combination to the next in lexicographic order.
/// Returns false if no more combinations exist.
fn next_combination(indices: &mut [usize], n: usize) -> bool {
    let k = indices.len();
    if k == 0 {
        return false;
    }
    // Find rightmost element that can be incremented
    let mut i = k;
    loop {
        if i == 0 {
            return false;
        }
        i -= 1;
        if indices[i] < n - k + i {
            indices[i] += 1;
            for j in (i + 1)..k {
                indices[j] = indices[j - 1] + 1;
            }
            return true;
        }
    }
}
