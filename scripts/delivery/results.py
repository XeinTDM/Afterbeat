"""Recompute delivery metrics from raw Win32 window-message timestamps."""

import argparse
from collections import defaultdict
import csv
import json
import math
from pathlib import Path
from statistics import median


LEFT_DOWN = 0x0201
LEFT_UP = 0x0202
MIN_P99_GAPS = 100
SMOKE_INITIAL_CLICKS = 8
NS_PER_SECOND = 1_000_000_000
NS_PER_MS = 1_000_000


def read_events(folder):
    with (folder / "events.csv").open(newline="", encoding="utf-8") as stream:
        return [
            (int(row["time_ns"]), int(row["message"]), int(row["tag"]))
            for row in csv.DictReader(stream)
        ]


def validate_edges(events, expected_tag, *, censored=False):
    """Require alternating down/up events so a missing release cannot pass."""
    expected = LEFT_DOWN
    previous = -1
    for timestamp, message, tag in events:
        if timestamp < previous or message != expected or tag & 0xFFFFFFFF != expected_tag:
            raise ValueError(f"Unexpected event at {timestamp} ns: message={message:#x}, tag={tag:#x}")
        expected = LEFT_UP if expected == LEFT_DOWN else LEFT_DOWN
        previous = timestamp
    if expected != LEFT_DOWN and not censored:
        raise ValueError("The final button down has no matching release.")
    return len(events) // 2


def input_tag(manifest, case):
    peer = case.get("peer")
    return manifest["peers"][peer]["input_tag"] if peer else manifest["input_tag"]


def trial_metrics(trial, events):
    case = trial["case"]
    stop = trial["stop_ns"]
    observed = [edge for edge in events if edge[0] < trial["observed_until_ns"]]
    if stop is None:
        return {
            "id": case["id"], "mode": case["mode"], "condition": case["condition"],
            "target_cps": case["rate"], "received_clicks": len(observed) // 2,
            "measurement_clicks": None, "spacing_gaps": None, "delivered_cps": None,
            "cpu_one_core_pct": None, "p99_spacing_error_ms": None, "worst_gap_ms": None,
            "stop_binding": None, "downs_after_stop": None, "stop_tail_ms": None, "stop_censored": None,
        }
    downs = [timestamp for timestamp, message, _ in observed if message == LEFT_DOWN]
    start, end = trial["measure_begin_ns"], trial["measure_end_ns"]
    measured = [timestamp for timestamp in downs if start <= timestamp < end]
    gaps = [(b - a) / NS_PER_MS for a, b in zip(measured, measured[1:])]
    expected = 1000 / case["rate"]
    spacing = sorted(abs(gap - expected) for gap in gaps)
    post_stop = [timestamp for timestamp, _, _ in observed if timestamp >= stop]
    cpu_start, cpu_end = trial["cpu_start"], trial["cpu_end"]
    cpu_elapsed = cpu_end[0] - cpu_start[0]
    return {
        "id": case["id"], "mode": case["mode"], "condition": case["condition"],
        "target_cps": case["rate"], "received_clicks": len(downs),
        "measurement_clicks": len(measured), "spacing_gaps": len(gaps),
        "delivered_cps": len(measured) * NS_PER_SECOND / (end - start),
        "cpu_one_core_pct": 100 * (cpu_end[1] - cpu_start[1]) / cpu_elapsed,
        "p99_spacing_error_ms": spacing[math.ceil(len(spacing) * 0.99) - 1]
        if len(spacing) >= MIN_P99_GAPS else None,
        "worst_gap_ms": max(gaps, default=None),
        "stop_binding": trial["stop_binding"],
        "downs_after_stop": sum(timestamp >= stop for timestamp in downs),
        "stop_tail_ms": (post_stop[-1] - stop) / NS_PER_MS if post_stop else 0.0,
        "stop_censored": bool(post_stop and post_stop[-1] >= trial["observed_until_ns"] - 175 * NS_PER_MS),
    }


def check_trial(trial, events, manifest):
    case = trial["case"]
    observed = [edge for edge in events if edge[0] < trial["observed_until_ns"]]
    metrics = trial_metrics(trial, observed)
    clicks = validate_edges(observed, input_tag(manifest, case), censored=metrics["stop_censored"])
    if manifest["command"] != "smoke":
        if not metrics["measurement_clicks"]:
            raise RuntimeError("The measurement window received no clicks.")
        if observed[0][0] < trial["start_ns"]:
            raise RuntimeError("The receiver got clicks before the start toggle.")
        return metrics
    if "count" in case:
        if clicks != case["count"]:
            raise RuntimeError(f"Counted run delivered {clicks} clicks, expected {case['count']}.")
        return metrics
    stop = trial["stop_ns"]
    if stop is None:
        raise RuntimeError("The smoke trial has no stop timestamp.")
    binding = trial["stop_binding"]
    limit = manifest["smoke_stop_ms"]
    if metrics["stop_censored"] or metrics["stop_tail_ms"] > limit:
        raise RuntimeError(f"Button edges continued more than {limit} ms after {binding}.")
    if case["mode"] == "held-button":
        if not (clicks == 1 and observed[0][0] < stop <= observed[1][0]):
            raise RuntimeError(f"The held button was not released by {binding}.")
    elif sum(timestamp < stop and message == LEFT_UP for timestamp, message, _ in observed) < SMOKE_INITIAL_CLICKS:
        raise RuntimeError(f"The receiver got fewer than {SMOKE_INITIAL_CLICKS} clicks before {binding}.")
    return metrics


def write_csv(path, rows):
    with path.open("w", newline="", encoding="utf-8") as stream:
        writer = csv.DictWriter(stream, fieldnames=rows[0].keys())
        writer.writeheader()
        writer.writerows(rows)


def replay(output):
    manifest = json.loads((output / "manifest.json").read_text(encoding="utf-8"))
    if manifest["schema_version"] != 1:
        raise ValueError("Unsupported delivery manifest version.")
    rows = []
    for case in manifest["cases"]:
        folder = output / case["id"]
        trial = json.loads((folder / "trial.json").read_text(encoding="utf-8"))
        if trial["status"] != "passed" or trial["case"] != case:
            raise ValueError(f"{case['id']} failed or does not match the manifest.")
        rows.append(check_trial(trial, read_events(folder), manifest))
    write_csv(output / "trials.csv", rows)
    if manifest["command"] in ("benchmark", "peers"):
        groups = defaultdict(list)
        for row in rows:
            groups[row["mode"], row["condition"], row["target_cps"]].append(row)
        summary = []
        for (mode, condition, rate), trials in sorted(groups.items()):
            delivered = [row["delivered_cps"] for row in trials]
            spacing = [row["p99_spacing_error_ms"] for row in trials if row["p99_spacing_error_ms"] is not None]
            summary.append({
                "mode": mode, "condition": condition, "target_cps": rate, "trials": len(trials),
                "delivered_cps_min": min(delivered), "delivered_cps_median": median(delivered),
                "delivered_cps_max": max(delivered),
                "cpu_one_core_pct_median": median(row["cpu_one_core_pct"] for row in trials),
                "p99_trials": len(spacing), "p99_spacing_error_ms_median": median(spacing) if spacing else None,
                "worst_stop_tail_ms": max(row["stop_tail_ms"] for row in trials),
                "censored_stop_trials": sum(row["stop_censored"] for row in trials),
            })
        write_csv(output / "summary.csv", summary)
    return rows


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run", type=Path)
    print(f"Recomputed {len(replay(parser.parse_args().run))} delivery trials.")
