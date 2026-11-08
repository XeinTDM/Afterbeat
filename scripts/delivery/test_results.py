import csv
import json
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

from results import LEFT_DOWN, LEFT_UP, NS_PER_MS, check_trial, replay, trial_metrics, validate_edges


TAG = 0x50434C4B


def trial_and_edges(clicks=101):
    case = {"id": "trial-0001", "mode": "Precise", "condition": "idle", "rate": 100}
    end = clicks * 10 * NS_PER_MS
    trial = {
        "case": case, "status": "passed", "stop_binding": "F7", "measure_begin_ns": 0, "measure_end_ns": end,
        "start_ns": 0,
        "stop_ns": end, "observed_until_ns": end + 500 * NS_PER_MS,
        "cpu_start": [0, 0], "cpu_end": [end, end // 10],
    }
    edges = [
        edge for index in range(clicks)
        for edge in ((index * 10 * NS_PER_MS, LEFT_DOWN, TAG), ((index * 10 + 1) * NS_PER_MS, LEFT_UP, TAG))
    ]
    return trial, edges


class DeliveryResults(unittest.TestCase):
    def test_missing_release_and_foreign_input_are_detected(self):
        with self.assertRaisesRegex(ValueError, "matching release"):
            validate_edges([(0, LEFT_DOWN, TAG)], TAG)
        with self.assertRaisesRegex(ValueError, "Unexpected event"):
            validate_edges([(0, LEFT_DOWN, 0), (1, LEFT_UP, TAG)], TAG)

    def test_p99_is_unavailable_below_100_gaps(self):
        trial, edges = trial_and_edges(100)
        row = trial_metrics(trial, edges)
        self.assertEqual(row["spacing_gaps"], 99)
        self.assertIsNone(row["p99_spacing_error_ms"])
        self.assertEqual(row["worst_gap_ms"], 10)

    def test_replay_reproduces_metrics_from_raw_edges(self):
        trial, edges = trial_and_edges()
        with TemporaryDirectory() as temp:
            root = Path(temp)
            folder = root / trial["case"]["id"]
            folder.mkdir()
            (root / "manifest.json").write_text(json.dumps({
                "schema_version": 1, "command": "benchmark", "input_tag": TAG, "cases": [trial["case"]],
            }), encoding="utf-8")
            (folder / "trial.json").write_text(json.dumps(trial), encoding="utf-8")
            with (folder / "events.csv").open("w", newline="", encoding="utf-8") as stream:
                writer = csv.writer(stream)
                writer.writerow(("time_ns", "message", "tag"))
                writer.writerows(edges)
            row, = replay(root)
            self.assertEqual(row["delivered_cps"], 100)
            self.assertEqual(row["cpu_one_core_pct"], 10)
            self.assertEqual(row["p99_spacing_error_ms"], 0)
            self.assertEqual(row["stop_tail_ms"], 0)
            self.assertTrue((root / "summary.csv").is_file())

    def test_peer_replay_accepts_only_the_pinned_peer_tag(self):
        trial, edges = trial_and_edges()
        trial["case"].update(mode="Peer", peer="fixture")
        trial["stop_binding"] = "F6"
        with TemporaryDirectory() as temp:
            root = Path(temp)
            folder = root / trial["case"]["id"]
            folder.mkdir()
            manifest = {
                "schema_version": 1, "command": "peers", "input_tag": TAG, "cases": [trial["case"]],
                "peers": {"fixture": {"input_tag": 0}},
            }
            (root / "manifest.json").write_text(json.dumps(manifest), encoding="utf-8")
            (folder / "trial.json").write_text(json.dumps(trial), encoding="utf-8")
            with (folder / "events.csv").open("w", newline="", encoding="utf-8") as stream:
                writer = csv.writer(stream)
                writer.writerow(("time_ns", "message", "tag"))
                writer.writerows((timestamp, message, 0) for timestamp, message, _ in edges)
            row, = replay(root)
            self.assertEqual(row["stop_binding"], "F6")
            self.assertTrue((root / "summary.csv").is_file())
            manifest["peers"]["fixture"]["input_tag"] = TAG
            (root / "manifest.json").write_text(json.dumps(manifest), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "Unexpected event"):
                replay(root)

    def test_replay_rejects_a_counted_trial_with_missing_clicks(self):
        trial, _ = trial_and_edges(12)
        trial["case"].update(mode="counted", count=12)
        trial["stop_ns"] = None
        with TemporaryDirectory() as temp:
            root = Path(temp)
            folder = root / trial["case"]["id"]
            folder.mkdir()
            (root / "manifest.json").write_text(json.dumps({
                "schema_version": 1, "command": "smoke", "input_tag": TAG, "cases": [trial["case"]],
            }), encoding="utf-8")
            (folder / "trial.json").write_text(json.dumps(trial), encoding="utf-8")
            (folder / "events.csv").write_text("time_ns,message,tag\n", encoding="utf-8")
            with self.assertRaisesRegex(RuntimeError, "delivered 0 clicks, expected 12"):
                replay(root)
            self.assertFalse((root / "trials.csv").exists())

    def test_unconfirmed_stop_keeps_a_censored_tail(self):
        trial, edges = trial_and_edges()
        edges.append((trial["observed_until_ns"] - 10 * NS_PER_MS, LEFT_DOWN, TAG))
        row = trial_metrics(trial, edges)
        self.assertTrue(row["stop_censored"])
        self.assertEqual(row["downs_after_stop"], 1)
        self.assertEqual(row["stop_tail_ms"], 490)

    def test_smoke_requires_held_button_release_after_f7(self):
        trial, _ = trial_and_edges()
        trial["case"]["mode"] = "held-button"
        manifest = {"command": "smoke", "smoke_stop_ms": 250, "input_tag": TAG}
        stop = trial["stop_ns"]
        check_trial(trial, [(0, LEFT_DOWN, TAG), (stop + NS_PER_MS, LEFT_UP, TAG)], manifest)
        with self.assertRaisesRegex(RuntimeError, "not released by F7"):
            check_trial(trial, [(0, LEFT_DOWN, TAG), (stop - NS_PER_MS, LEFT_UP, TAG)], manifest)

    def test_emergency_stop_requires_clicks_before_f7(self):
        trial, edges = trial_and_edges(8)
        trial["case"]["mode"] = "emergency-stop"
        manifest = {"command": "smoke", "smoke_stop_ms": 250, "input_tag": TAG}
        self.assertEqual(check_trial(trial, edges, manifest)["received_clicks"], 8)
        with self.assertRaisesRegex(RuntimeError, "fewer than 8 clicks before F7"):
            check_trial(trial, [], manifest)
