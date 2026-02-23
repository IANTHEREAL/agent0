//! Tests for the EXPLAIN module.
//!
//! The primary EXPLAIN path (physical_plan_to_plan_node + format_plan_text)
//! is exercised via optimizer/physical_planner/tests.rs which constructs
//! PhysicalPlan trees and verifies EXPLAIN output end-to-end.

use super::*;

#[test]
fn test_format_plan_text_result_node() {
    let plan = PlanNode::Result {
        cost: PlanCost::default(),
    };
    let output = format_plan_text(&plan, 0);
    assert!(output.contains("Result"));
}

#[test]
fn test_format_plan_text_seq_scan() {
    let plan = PlanNode::SeqScan {
        table_name: "users".to_string(),
        alias: None,
        filter: Some("name = 'Alice'".to_string()),
        cost: PlanCost {
            startup: 0.0,
            total: 10.0,
            rows: 1000,
            width: 40,
        },
    };
    let output = format_plan_text(&plan, 0);
    assert!(output.contains("Seq Scan on users"));
    assert!(output.contains("Filter:"));
}

#[test]
fn test_format_plan_text_index_scan() {
    let plan = PlanNode::IndexScan {
        table_name: "users".to_string(),
        alias: None,
        index_name: "users_pkey".to_string(),
        index_cond: Some("id = 1".to_string()),
        filter: None,
        cost: PlanCost {
            startup: 0.15,
            total: 0.25,
            rows: 1,
            width: 40,
        },
    };
    let output = format_plan_text(&plan, 0);
    assert!(output.contains("Index Scan using users_pkey on users"));
    assert!(output.contains("Index Cond:"));
}
