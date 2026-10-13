#!/usr/bin/env python3
"""Measure Quill's Cargo-workspace type-check phase at different worker caps.

This profiles an already-built Quill binary. `QUILL_JOBS` holds parser
concurrency fixed while `QUILL_TYPECK_JOBS` changes the checker pool. It is not
a Cargo build or an equivalent-rustc comparison.
"""

from __future__ import annotations

import datetime as dt
import argparse
import json
import os
import platform
import shutil
import statistics
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RESULTS = ROOT / "benchmarks" / "results"
FIXTURE = ROOT / "benchmarks" / "catalog-flow" / "Cargo.toml"
COMPILER = ROOT / "target" / "release" / "quillc"


def median(rows: list[dict[str, float | int]]) -> dict[str, float]:
    return {
        field: statistics.median(float(row[field]) for row in rows)
        for field in ("wall_seconds", "cpu_seconds", "peak_rss_kib")
    }


JOB_SETTINGS = (1, 4, 8, 12)


def run(jobs: int, round_number: int) -> dict[str, float | int]:
    env = os.environ.copy()
    env["QUILL_JOBS"] = str(os.cpu_count() or 1)
    env["QUILL_TYPECK_JOBS"] = str(jobs)
    cache_root = Path(tempfile.mkdtemp(prefix=f"quill-typeck-cache-{round_number}-{jobs}-"))
    env["QUILL_CACHE_DIR"] = str(cache_root)
    command = [
        str(COMPILER),
        "cargo-check-subset",
        str(FIXTURE),
    ]
    with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
        started = time.perf_counter()
        child = os.fork()
        if child == 0:
            os.chdir(ROOT)
            os.dup2(stdout.fileno(), 1)
            os.dup2(stderr.fileno(), 2)
            os.execvpe(command[0], command, env)
        waited_pid, status, usage = os.wait4(child, 0)
        wall = time.perf_counter() - started
        stdout.seek(0)
        stderr.seek(0)
        stdout_text = stdout.read().decode()
        stderr_text = stderr.read().decode()
    shutil.rmtree(cache_root, ignore_errors=True)
    exit_code = os.waitstatus_to_exitcode(status)
    if waited_pid != child or exit_code:
        raise SystemExit(
            f"measurement failed for QUILL_TYPECK_JOBS={jobs}:\n{stdout_text}\n{stderr_text}"
        )
    cpu = usage.ru_utime + usage.ru_stime
    print(f"jobs={jobs} round={round_number}: wall={wall:.3f}s cpu={cpu:.2f}s rss={usage.ru_maxrss} KiB")
    summary = stdout_text.strip().splitlines()[-1]
    print(f"  {summary}")
    return {
        "round": round_number,
        "wall_seconds": wall,
        "cpu_seconds": cpu,
        "peak_rss_kib": usage.ru_maxrss,
        "exit_code": exit_code,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rounds", type=int, default=5)
    args = parser.parse_args()
    if args.rounds < 1:
        parser.error("--rounds must be at least 1")
    if not COMPILER.is_file():
        raise SystemExit("build target/release/quillc before running this measurement")
    source_files = list((FIXTURE.parent).glob("*/src/**/*.rs"))
    lines = sum(len(path.read_text().splitlines()) for path in source_files)
    rows: dict[int, list[dict[str, float | int]]] = {jobs: [] for jobs in JOB_SETTINGS}
    # Interleave worker settings to reduce bias from thermal and background drift.
    for round_number in range(1, args.rounds + 1):
        order = JOB_SETTINGS if round_number % 2 else tuple(reversed(JOB_SETTINGS))
        for jobs in order:
            rows[jobs].append(run(jobs, round_number))
    output = {
        "schema_version": 1,
        "timestamp_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "experiment": "type-check worker scaling with parser concurrency held fixed",
        "measurement": "Quill cargo-check-subset type-check phase with parser concurrency fixed and a fresh query-cache directory each run; not a Cargo build or rustc compile comparison",
        "host": {
            "platform": platform.platform(),
            "machine": platform.machine(),
            "logical_cpus": os.cpu_count(),
        },
        "workload": {
            "name": "catalog-flow",
            "source_files": len(source_files),
            "source_lines": lines,
            "cargo_manifest": str(FIXTURE.relative_to(ROOT)),
        },
        "command": "target/release/quillc cargo-check-subset benchmarks/catalog-flow/Cargo.toml",
        "parser_worker_cap": os.cpu_count(),
        "rounds_per_setting": args.rounds,
        "method": f"{args.rounds} interleaved fresh-process runs per QUILL_TYPECK_JOBS setting while QUILL_JOBS is fixed at logical CPU count; a new QUILL_CACHE_DIR forces cold query misses each run; perf_counter wall and Linux wait4 child user+system CPU and peak RSS.",
        "settings": [
            {"jobs": jobs, "runs": rows[jobs], "median": median(rows[jobs])}
            for jobs in JOB_SETTINGS
        ],
    }
    RESULTS.mkdir(exist_ok=True)
    path = RESULTS / f"parallel-typeck-{dt.datetime.now().strftime('%Y%m%d-%H%M%S')}.json"
    path.write_text(json.dumps(output, indent=2) + "\n")
    print(f"results: {path.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
