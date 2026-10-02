#!/usr/bin/env python3
"""Contract tests for the token-footprint baseline gate."""

import copy
import importlib.util
import json
import pathlib
import unittest


SCRIPT = pathlib.Path(__file__).with_name("check_token_footprint.py")
SPEC = importlib.util.spec_from_file_location("check_token_footprint", SCRIPT)
checker = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(checker)


def fixture(value=100):
    metric = {
        "request": {"bytes": value, "tokens": value},
        "response": {"bytes": value, "tokens": value},
        "total": {"bytes": value * 2, "tokens": value * 2},
        "model_visible": {"bytes": value, "tokens": value},
    }
    return {
        "schema_version": 1,
        "tokenizer": {
            "name": "o200k_base",
            "implementation": "tiktoken-rs",
            "version": "0.12.0",
            "vocab_fingerprint": "sha256:fixture",
        },
        "fixture_fingerprint": "sha256:fixture",
        "flows": {
            "publish_to_zone": {
                "flow_version": 1,
                "flow_fingerprint": "sha256:zone",
                "operations": {"publish_to_zone": copy.deepcopy(metric)},
                "total": copy.deepcopy(metric["total"]),
                "model_visible": copy.deepcopy(metric["model_visible"]),
            },
            "portal_projection": {
                "flow_version": 1,
                "flow_fingerprint": "sha256:portal",
                "operations": {"attach": copy.deepcopy(metric)},
                "total": copy.deepcopy(metric["total"]),
                "model_visible": copy.deepcopy(metric["model_visible"]),
            },
            "publish_to_widget": {
                "flow_version": 1,
                "flow_fingerprint": "sha256:widget",
                "operations": {"publish_to_widget": copy.deepcopy(metric)},
                "total": copy.deepcopy(metric["total"]),
                "model_visible": copy.deepcopy(metric["model_visible"]),
            },
        },
    }


def approve(document, budget=10_000):
    document["approval"] = {
        "status": "owner_approved",
        "decision_reference": "hud-test-decision",
    }
    document["budgets"] = {
        "flows": {name: budget for name in document["flows"]},
    }


class GateTests(unittest.TestCase):
    def test_exact_five_percent_is_warning_but_six_percent_fails(self):
        baseline = fixture()
        approve(baseline)
        at_limit = fixture(105)
        report = checker.compare(at_limit, baseline)
        self.assertEqual(report["status"], "warning")
        self.assertFalse(report["regressions"])
        self.assertEqual(report["warnings"][0]["absolute_delta"], 5)
        self.assertEqual(report["warnings"][0]["percentage_delta"], 5.0)

        over_limit = fixture(106)
        report = checker.compare(over_limit, baseline)
        self.assertEqual(report["status"], "failed")
        self.assertTrue(report["regressions"])
        self.assertEqual(report["regressions"][0]["absolute_delta"], 6)
        self.assertEqual(report["regressions"][0]["percentage_delta"], 6.0)

    def test_compares_every_operation_direction_and_flow_total(self):
        baseline = fixture()
        approve(baseline)
        measurement = fixture()
        measurement["flows"]["portal_projection"]["operations"]["attach"]["response"][
            "tokens"
        ] = 106
        measurement["flows"]["portal_projection"]["operations"]["attach"]["total"][
            "tokens"
        ] = 206
        measurement["flows"]["portal_projection"]["total"]["tokens"] = 206
        report = checker.compare(measurement, baseline)
        regression_paths = {entry["path"] for entry in report["regressions"]}
        warning_paths = {entry["path"] for entry in report["warnings"]}
        self.assertEqual(
            regression_paths,
            {"portal_projection.operations.attach.response.tokens"},
        )
        self.assertEqual(
            warning_paths,
            {
                "portal_projection.operations.attach.total.tokens",
                "portal_projection.total.tokens",
            },
        )

    def test_fingerprint_drift_is_incompatible_not_a_regression(self):
        baseline = fixture()
        approve(baseline)
        measurement = fixture()
        measurement["flows"]["publish_to_zone"]["flow_fingerprint"] = "sha256:changed"
        report = checker.compare(measurement, baseline)
        self.assertEqual(report["status"], "baseline_incompatible")
        self.assertFalse(report["regressions"])

    def test_missing_flow_fingerprint_on_both_sides_fails_closed(self):
        baseline = fixture()
        approve(baseline)
        measurement = fixture()
        del measurement["flows"]["portal_projection"]["flow_fingerprint"]
        del baseline["flows"]["portal_projection"]["flow_fingerprint"]

        report = checker.compare(measurement, baseline)

        self.assertEqual(report["status"], "baseline_incompatible")
        self.assertIn(
            "missing or invalid flow fingerprint: measurement:portal_projection",
            report["incompatibilities"],
        )
        self.assertIn(
            "missing or invalid flow fingerprint: baseline:portal_projection",
            report["incompatibilities"],
        )

    def test_empty_or_non_string_flow_fingerprint_fails_closed(self):
        for invalid_fingerprint in ("", "   ", 1):
            with self.subTest(invalid_fingerprint=invalid_fingerprint):
                baseline = fixture()
                approve(baseline)
                measurement = fixture()
                measurement["flows"]["portal_projection"][
                    "flow_fingerprint"
                ] = invalid_fingerprint
                baseline["flows"]["portal_projection"][
                    "flow_fingerprint"
                ] = invalid_fingerprint

                report = checker.compare(measurement, baseline)

                self.assertEqual(report["status"], "baseline_incompatible")
                self.assertIn(
                    "missing or invalid flow fingerprint: measurement:portal_projection",
                    report["incompatibilities"],
                )
                self.assertIn(
                    "missing or invalid flow fingerprint: baseline:portal_projection",
                    report["incompatibilities"],
                )

    def test_model_visible_tokens_over_budget_fail(self):
        baseline = fixture()
        approve(baseline, budget=100)
        self.assertEqual(checker.compare(fixture(), baseline)["status"], "passed")
        over = fixture()
        over["flows"]["publish_to_zone"]["operations"]["publish_to_zone"]["model_visible"][
            "tokens"
        ] = 101
        over["flows"]["publish_to_zone"]["model_visible"]["tokens"] = 101
        report = checker.compare(over, baseline)
        self.assertEqual(report["status"], "failed")
        self.assertEqual(
            report["budget_violations"],
            [{"flow": "publish_to_zone", "budget": 100, "measured": 101}],
        )

    def test_missing_budgets_fail_closed(self):
        baseline = fixture()
        approve(baseline)
        del baseline["budgets"]
        report = checker.compare(fixture(), baseline)
        self.assertEqual(report["status"], "baseline_incompatible")
        self.assertIn("budgets", " ".join(report["incompatibilities"]))

    def test_unapproved_baseline_fails_closed(self):
        report = checker.compare(fixture(), fixture())
        self.assertEqual(report["status"], "baseline_incompatible")
        self.assertIn("owner-approved", " ".join(report["incompatibilities"]))

    def test_pending_review_baseline_warns_but_compares(self):
        baseline = fixture()
        approve(baseline)
        baseline["approval"]["status"] = "pending_owner_review"
        report = checker.compare(fixture(), baseline)
        self.assertEqual(report["status"], "warning")
        self.assertEqual(report["approval"], "pending_owner_review")
        approve(baseline, budget=1)
        baseline["approval"]["status"] = "pending_owner_review"
        self.assertEqual(checker.compare(fixture(), baseline)["status"], "failed")

    def test_unknown_approval_status_fails_closed(self):
        baseline = fixture()
        approve(baseline)
        baseline["approval"]["status"] = "approved"
        report = checker.compare(fixture(), baseline)
        self.assertEqual(report["status"], "baseline_incompatible")

    def test_missing_metric_fails_closed_as_incompatible(self):
        baseline = fixture()
        approve(baseline)
        measurement = fixture()
        del measurement["flows"]["publish_to_widget"]["total"]["tokens"]
        report = checker.compare(measurement, baseline)
        self.assertEqual(report["status"], "baseline_incompatible")
        self.assertIn("missing or invalid integer metric", " ".join(report["incompatibilities"]))

    def test_approved_baseline_requires_decision_reference(self):
        baseline = fixture()
        baseline["approval"] = {"status": "owner_approved"}
        report = checker.compare(fixture(), baseline)
        self.assertEqual(report["status"], "baseline_incompatible")
        self.assertIn("decision reference", " ".join(report["incompatibilities"]))

    def test_flow_version_drift_is_incompatible(self):
        baseline = fixture()
        approve(baseline)
        measurement = fixture()
        measurement["flows"]["portal_projection"]["flow_version"] = 2
        report = checker.compare(measurement, baseline)
        self.assertEqual(report["status"], "baseline_incompatible")
        self.assertIn("flow version changed", " ".join(report["incompatibilities"]))

    def test_inconsistent_operation_and_flow_totals_fail_closed(self):
        baseline = fixture()
        approve(baseline)
        measurement = fixture()
        measurement["flows"]["publish_to_zone"]["operations"]["publish_to_zone"][
            "total"
        ]["tokens"] += 1
        measurement["flows"]["publish_to_widget"]["total"]["bytes"] += 1
        report = checker.compare(measurement, baseline)
        self.assertEqual(report["status"], "baseline_incompatible")
        reasons = " ".join(report["incompatibilities"])
        self.assertIn("operation total mismatch", reasons)
        self.assertIn("flow total mismatch", reasons)


class CandidatePacketTests(unittest.TestCase):
    def setUp(self):
        self.root = SCRIPT.parents[2]
        self.candidate = json.loads(
            (self.root / "scripts/ci/token_footprint_baseline.json").read_text(
                encoding="utf-8"
            )
        )

    def test_candidate_is_owner_approved(self):
        self.assertEqual(self.candidate["approval"]["status"], "owner_approved")

    def test_baseline_budgets_match_api_targets(self):
        self.assertEqual(
            self.candidate["budgets"]["flows"],
            {
                "tools_list": 900,
                "discover": 150,
                "zone_publish": 80,
                "widget_publish": 80,
                "portal": 250,
                "error": 60,
            },
        )

    def test_candidate_is_accepted_by_fail_closed_gate(self):
        report = checker.compare(copy.deepcopy(self.candidate), self.candidate)
        self.assertEqual(report["status"], "passed")
        self.assertFalse(report["incompatibilities"])
        self.assertFalse(report["warnings"] or report["regressions"])

    def test_owner_approval_makes_candidate_pass(self):
        approved = copy.deepcopy(self.candidate)
        approved["approval"]["status"] = "owner_approved"
        report = checker.compare(copy.deepcopy(self.candidate), approved)
        self.assertEqual(report["status"], "passed")


if __name__ == "__main__":
    unittest.main()
