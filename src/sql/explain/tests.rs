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
    let output = format_plan_text(&plan, 0, false);
    assert!(output.contains("Result"));
}

#[test]
fn test_format_plan_text_seq_scan() {
    let plan = PlanNode::SeqScan {
        table_name: "users".to_string(),
        alias: None,
        filter: Some("name = 'Alice'".to_string()),
        annotations: PlanAnnotations::default(),
        cost: PlanCost {
            startup: 0.0,
            total: 10.0,
            rows: 1000,
            width: 40,
        },
    };
    let output = format_plan_text(&plan, 0, false);
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
        annotations: PlanAnnotations::default(),
        cost: PlanCost {
            startup: 0.15,
            total: 0.25,
            rows: 1,
            width: 40,
        },
    };
    let output = format_plan_text(&plan, 0, false);
    assert!(output.contains("Index Scan using users_pkey on users"));
    assert!(output.contains("Index Cond:"));
}

#[test]
fn test_format_plan_text_tikv_annotations_default_and_verbose() {
    let plan = PlanNode::IndexScan {
        table_name: "users".to_string(),
        alias: None,
        index_name: "users_email_idx".to_string(),
        index_cond: Some("(active = true)".to_string()),
        filter: None,
        annotations: PlanAnnotations {
            task: Some("cop[tikv]".to_string()),
            output: Some(vec!["id".to_string(), "email".to_string()]),
            pushed_down: vec![
                "Filter".to_string(),
                "Project".to_string(),
                "Limit".to_string(),
            ],
            storage_access: Some("point ('a@example.com')".to_string()),
            storage_limit: Some(10),
        },
        cost: PlanCost {
            startup: 0.15,
            total: 0.25,
            rows: 1,
            width: 80,
        },
    };

    let output = format_plan_text(&plan, 0, false);
    assert!(output.contains("Index Scan using users_email_idx on users"));
    assert!(output.contains("Index Cond: (active = true)"));
    assert!(output.contains("DB9 Cop Access: point ('a@example.com')"));
    assert!(output.contains("DB9 Cop Filter: (active = true)"));
    assert!(output.contains("DB9 Cop Output: id, email"));
    assert!(output.contains("DB9 Cop Limit: 10"));
    assert!(!output.contains("Task: cop[tikv]"));

    let verbose_output = format_plan_text(&plan, 0, true);
    assert!(verbose_output.contains("DB9 Cop Access: point ('a@example.com')"));
    assert!(verbose_output.contains("DB9 Cop Filter: (active = true)"));
    assert!(verbose_output.contains("DB9 Cop Output: id, email"));
    assert!(verbose_output.contains("DB9 Cop Limit: 10"));
    assert!(!verbose_output.contains("Pushed Down:"));
}
