import json
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from sticker_diagnostics import STAGES, sticker_diagnostics


class StickerDiagnosticTests(unittest.TestCase):
    def test_deadline_reports_the_last_known_await_without_values(self):
        output = ("private-server-response\nMORPHEUS_STICKER_STAGE catalog\n"
                  "MORPHEUS_STICKER_STAGE room_outcome\n"
                  "MORPHEUS_STICKER_FAILURE scenario_deadline\n"
                  "MORPHEUS_STICKER_CLEANUP_FAILURE failed\n")
        self.assertEqual(sticker_diagnostics(output), {
            "stage": "room_outcome", "failure": "scenario_deadline",
            "cleanup": "failed", "invalid_marker": False,
        })
        self.assertNotIn("private", json.dumps(sticker_diagnostics(output)))

    def test_assertion_and_unknown_failures_withhold_the_stage(self):
        for suffix in ("MORPHEUS_STICKER_FAILURE check_failed\n", ""):
            report = sticker_diagnostics("MORPHEUS_STICKER_STAGE consent\n" + suffix)
            self.assertEqual(report["stage"], "withheld")
            self.assertNotIn("consent", json.dumps(report))

    def test_arbitrary_markers_cannot_reflect_paths_urls_or_credentials(self):
        for value in ("mxc://private/key", "/private/profile", "private-token",
                      "ROOM_OUTCOME", "room_outcome extra", "", "x" * 100_000):
            with self.subTest(length=len(value)):
                output = ("MORPHEUS_STICKER_STAGE " + value + "\n"
                          "MORPHEUS_STICKER_FAILURE " + value + "\n"
                          "MORPHEUS_STICKER_CLEANUP_FAILURE " + value + "\n")
                report = sticker_diagnostics(output)
                self.assertTrue(report["invalid_marker"])
                self.assertEqual(report["stage"], "withheld")
                self.assertEqual(report["failure"], "unknown")
                self.assertNotIn("private", json.dumps(report))
                self.assertLess(len(json.dumps(report)), 160)

    def test_each_known_stage_is_closed_and_no_marker_means_no_record(self):
        for stage in STAGES:
            report = sticker_diagnostics(f"MORPHEUS_STICKER_STAGE {stage}\n"
                                         "MORPHEUS_STICKER_FAILURE scenario_deadline\n")
            self.assertEqual(report["stage"], stage)
        self.assertEqual(sticker_diagnostics("private payload\n"), {})
        self.assertEqual(sticker_diagnostics("quoted MORPHEUS_STICKER_STAGE catalog\n"), {})


if __name__ == "__main__":
    unittest.main()
