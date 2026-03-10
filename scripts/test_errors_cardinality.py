#!/usr/bin/env python3
"""Unit tests for .errors cardinality enforcement (issue #1675).

Verifies that match_errors() enforces full expected-error cardinality:
  - Every expected pattern must be matched by a distinct actual error line.
  - Every actual error line must be matched by some expected pattern.
  - Duplicate expected patterns each consume a separate actual line.
"""
import importlib.util
import unittest
from pathlib import Path

MODULE_PATH = Path(__file__).with_name("integration_test.py")
SPEC = importlib.util.spec_from_file_location("integration_test_module", MODULE_PATH)
integration_test = importlib.util.module_from_spec(SPEC)
assert SPEC is not None and SPEC.loader is not None
SPEC.loader.exec_module(integration_test)

match_errors = integration_test.match_errors
match_errors_unified = integration_test.match_errors_unified


class TestMatchErrorsCardinality(unittest.TestCase):
    # --- positive cases (should pass) ---

    def test_exact_match_one_to_one(self):
        """Each expected pattern matches exactly one actual error."""
        expected = ["column not found", "table does not exist"]
        actual = [
            'ERROR: column not found "foo"',
            'ERROR: table does not exist "bar"',
        ]
        result = match_errors(expected, actual)
        self.assertTrue(result.ok)
        self.assertEqual(result.unexpected, [])
        self.assertEqual(result.unmatched_expected, [])

    def test_duplicate_patterns_matched_by_distinct_actuals(self):
        """Duplicate expected patterns each consume a separate actual line."""
        expected = ["missing_col", "missing_col", "missing_col"]
        actual = [
            "ERROR: missing_col in expression A",
            "ERROR: missing_col in expression B",
            "ERROR: missing_col in expression C",
        ]
        result = match_errors(expected, actual)
        self.assertTrue(result.ok)

    def test_single_pattern_single_actual(self):
        expected = ["syntax error"]
        actual = ["ERROR: syntax error at or near SELECT"]
        result = match_errors(expected, actual)
        self.assertTrue(result.ok)

    # --- negative: unmatched expected patterns ---

    def test_missing_expected_pattern(self):
        """One expected pattern has no matching actual → should fail."""
        expected = ["column not found", "schema does not exist"]
        actual = ['ERROR: column not found "foo"']
        result = match_errors(expected, actual)
        self.assertFalse(result.ok)
        self.assertEqual(result.unmatched_expected, ["schema does not exist"])
        self.assertEqual(result.unexpected, [])

    def test_all_expected_patterns_missing(self):
        """No actual errors match any expected pattern."""
        expected = ["alpha", "beta"]
        actual = ["ERROR: gamma happened"]
        result = match_errors(expected, actual)
        self.assertFalse(result.ok)
        self.assertIn("alpha", result.unmatched_expected)
        self.assertIn("beta", result.unmatched_expected)

    def test_duplicate_patterns_insufficient_actuals(self):
        """5 identical expected patterns but only 3 actual matches → 2 unmatched."""
        expected = ["missing_col"] * 5
        actual = [
            "ERROR: missing_col in A",
            "ERROR: missing_col in B",
            "ERROR: missing_col in C",
        ]
        result = match_errors(expected, actual)
        self.assertFalse(result.ok)
        self.assertEqual(len(result.unmatched_expected), 2)
        self.assertEqual(result.unexpected, [])

    # --- negative: unexpected actual errors ---

    def test_unexpected_actual_error(self):
        """Actual error not covered by any expected pattern."""
        expected = ["column not found"]
        actual = [
            'ERROR: column not found "foo"',
            "ERROR: unexpected division by zero",
        ]
        result = match_errors(expected, actual)
        self.assertFalse(result.ok)
        self.assertEqual(result.unmatched_expected, [])
        self.assertEqual(len(result.unexpected), 1)
        self.assertIn("division by zero", result.unexpected[0])

    # --- edge cases ---

    def test_empty_expected_and_actual(self):
        result = match_errors([], [])
        self.assertTrue(result.ok)

    def test_empty_actual_with_expected(self):
        result = match_errors(["some error"], [])
        self.assertFalse(result.ok)
        self.assertEqual(result.unmatched_expected, ["some error"])

    def test_empty_expected_with_actual(self):
        result = match_errors([], ["ERROR: boom"])
        self.assertFalse(result.ok)
        self.assertEqual(result.unexpected, ["ERROR: boom"])

    def test_pattern_order_does_not_matter(self):
        """Patterns match regardless of order in expected list."""
        expected = ["beta", "alpha"]
        actual = ["ERROR: alpha happened", "ERROR: beta happened"]
        result = match_errors(expected, actual)
        self.assertTrue(result.ok)

    def test_ambiguous_patterns_assigned_correctly(self):
        """When a pattern could match multiple actuals, matching finds valid assignment."""
        expected = ["err", "err", "unique"]
        actual = [
            "ERROR: err first",
            "ERROR: err second",
            "ERROR: unique thing",
        ]
        result = match_errors(expected, actual)
        self.assertTrue(result.ok)

    def test_overlapping_substring_patterns(self):
        """Broad pattern must not steal the only actual that a specific pattern needs.

        Regression test for P1 review finding: greedy first-fit fails here,
        but bipartite matching finds the valid assignment:
          "err"   -> "ERROR: err B happened"
          "err A" -> "ERROR: err A happened"
        """
        expected = ["err", "err A"]
        actual = ["ERROR: err A happened", "ERROR: err B happened"]
        result = match_errors(expected, actual)
        self.assertTrue(result.ok)
        self.assertEqual(result.unexpected, [])
        self.assertEqual(result.unmatched_expected, [])

    def test_overlapping_patterns_reversed_order(self):
        """Same overlap case with expected order reversed — still passes."""
        expected = ["err A", "err"]
        actual = ["ERROR: err A happened", "ERROR: err B happened"]
        result = match_errors(expected, actual)
        self.assertTrue(result.ok)

    def test_overlapping_patterns_no_valid_assignment(self):
        """When no valid assignment exists, overlapping patterns correctly fail."""
        expected = ["err A", "err B"]
        actual = ["ERROR: err A happened", "ERROR: err C happened"]
        result = match_errors(expected, actual)
        self.assertFalse(result.ok)
        self.assertEqual(result.unmatched_expected, ["err B"])
        self.assertEqual(result.unexpected, ["ERROR: err C happened"])


class TestMatchErrorsUnifiedCrossPhaseOverlap(unittest.TestCase):
    """Regression tests for cross-phase overlap (PR #1677, block round #4).

    Two-phase matching (match expected first, then divergence on leftovers)
    is order-dependent when an expected pattern and a divergence pattern
    compete for the same actual error.  Unified matching eliminates this.
    """

    def test_cross_phase_overlap_actuals_order1(self):
        """Broad expected 'err' must not steal the actual that divergence 'err A' needs."""
        expected = ["err"]
        divergence = ["err A"]
        actuals = ["ERROR: err A happened", "ERROR: err B happened"]
        result = match_errors_unified(expected, divergence, actuals)
        self.assertTrue(result.ok, f"unexpected={result.unexpected}, unmatched={result.unmatched_expected}")

    def test_cross_phase_overlap_actuals_order2(self):
        """Same overlap with actual order reversed — must still pass."""
        expected = ["err"]
        divergence = ["err A"]
        actuals = ["ERROR: err B happened", "ERROR: err A happened"]
        result = match_errors_unified(expected, divergence, actuals)
        self.assertTrue(result.ok, f"unexpected={result.unexpected}, unmatched={result.unmatched_expected}")

    def test_cross_phase_overlap_expected_order_reversed(self):
        """Expected and divergence patterns swapped in specificity."""
        expected = ["err A"]
        divergence = ["err"]
        actuals = ["ERROR: err A happened", "ERROR: err B happened"]
        result = match_errors_unified(expected, divergence, actuals)
        self.assertTrue(result.ok, f"unexpected={result.unexpected}, unmatched={result.unmatched_expected}")

    def test_cross_phase_no_valid_assignment(self):
        """When no valid assignment exists, unified matching correctly fails."""
        expected = ["err A"]
        divergence = ["err B"]
        actuals = ["ERROR: err A happened", "ERROR: err C happened"]
        result = match_errors_unified(expected, divergence, actuals)
        self.assertFalse(result.ok)
        self.assertEqual(result.unmatched_expected, [])
        self.assertEqual(result.unexpected, ["ERROR: err C happened"])

    def test_unified_expected_only(self):
        """Unified matching with no divergence patterns behaves like match_errors."""
        expected = ["column not found", "table does not exist"]
        actuals = ['ERROR: column not found "foo"', 'ERROR: table does not exist "bar"']
        result = match_errors_unified(expected, [], actuals)
        self.assertTrue(result.ok)

    def test_unified_divergence_only(self):
        """Unified matching with no expected patterns — divergence absorbs all."""
        divergence = ["column not found"]
        actuals = ['ERROR: column not found "foo"']
        result = match_errors_unified([], divergence, actuals)
        self.assertTrue(result.ok)

    def test_unified_divergence_only_unexpected(self):
        """Divergence-only with unmatched actual — reports unexpected."""
        divergence = ["column not found"]
        actuals = ['ERROR: column not found "foo"', "ERROR: boom"]
        result = match_errors_unified([], divergence, actuals)
        self.assertFalse(result.ok)
        self.assertEqual(result.unexpected, ["ERROR: boom"])

    def test_unified_empty(self):
        """Both empty lists — trivially passes."""
        result = match_errors_unified([], [], [])
        self.assertTrue(result.ok)

    def test_unified_multiple_overlapping_patterns(self):
        """Multiple expected + divergence patterns with shared substring overlap."""
        expected = ["err", "err X"]
        divergence = ["err Y"]
        actuals = [
            "ERROR: err X happened",
            "ERROR: err Y happened",
            "ERROR: err Z happened",
        ]
        result = match_errors_unified(expected, divergence, actuals)
        # "err X" → actual[0], "err" → actual[2], "err Y" → actual[1]
        self.assertTrue(result.ok, f"unexpected={result.unexpected}, unmatched={result.unmatched_expected}")


if __name__ == "__main__":
    unittest.main()
