#!/usr/bin/env python3
"""Measure parallel lowering of independent reachable helper bodies."""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import platform
import statistics
import subprocess
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RESULTS = ROOT / "benchmarks" / "results"
COMPILER = ROOT / "target" / "release" / "quillc"
GENERATOR = ROOT / "benchmarks" / "generate_reachable_helpers.py"


def measure(source: Path, output: Path, workers: int, round_number: int) -> dict[str, float | int]:
    env = os.environ.copy()
    env["QUILL_JOBS"] = str(os.cpu_count() or 1)
    env["QUILL_TYPECK_JOBS"] = str(os.cpu_count() or 1)
    env["QUILL_CODEGEN_JOBS"] = str(workers)
    command = [str(COMPILER), "build-subset", str(source), "-o", str(output)]
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
    exit_code = os.waitstatus_to_exitcode(status)
    if waited_pid != child or exit_code:
        raise SystemExit(
            f"codegen workers={workers} round={round_number} failed:\n"
            f"{stdout_text}\n{stderr_text}"
        )
    run = subprocess.run([str(output)], check=False)
    if run.returncode != 42:
        raise SystemExit(f"generated program exited {run.returncode}, expected 42")
    row = {
        "round": round_number,
        "codegen_workers": workers,
        "wall_seconds": wall,
        "cpu_seconds": usage.ru_utime + usage.ru_stime,
        "peak_rss_kib": usage.ru_maxrss,
        "output_bytes": output.stat().st_size,
        "program_exit_code": run.returncode,
    }
    print(
        f"round={round_number} workers={workers} wall={wall:.3f}s "
        f"cpu={row['cpu_seconds']:.2f}s rss={usage.ru_maxrss} KiB",
        flush=True,
    )
    return row


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rounds", type=int, default=5)
    parser.add_argument("--helpers", type=int, default=800)
    args = parser.parse_args()
    if args.rounds < 1 or args.helpers < 1:
        parser.error("--rounds and --helpers must be positive")
    if not COMPILER.is_file():
        raise SystemExit("build target/release/quillc before running this experiment")

    source = Path(f"/tmp/quill-reachable-{os.getpid()}.rs")
    rustc_output = Path(f"/tmp/quill-reachable-rustc-{os.getpid()}")
    subprocess.run(["python3", str(GENERATOR), str(source), "--helpers", str(args.helpers)], check=True)
    rustc = subprocess.run(["rustc", "--edition=2024", str(source), "-o", str(rustc_output)], check=False)
    if rustc.returncode or subprocess.run([str(rustc_output)]).returncode != 42:
        raise SystemExit("rustc oracle did not produce the expected exit code 42")

    source_bytes = source.read_bytes()
    source_lines = len(source_bytes.splitlines())
    source_sha256 = hashlib.sha256(source_bytes).hexdigest()
    workers = sorted({1, min(4, os.cpu_count() or 1), min(8, os.cpu_count() or 1), os.cpu_count() or 1})
    rows = []
    try:
        for round_number in range(1, args.rounds + 1):
            order = workers if round_number % 2 else list(reversed(workers))
            for count in order:
                output = Path(f"/tmp/quill-reachable-{os.getpid()}-{round_number}-{count}")
                rows.append(measure(source, output, count, round_number))
                output.unlink(missing_ok=True)
    finally:
        source.unlink(missing_ok=True)
        rustc_output.unlink(missing_ok=True)

    summaries = {}
    for count in workers:
        selected = [row for row in rows if row["codegen_workers"] == count]
        summaries[str(count)] = {
            key: statistics.median(float(row[key]) for row in selected)
            for key in ("wall_seconds", "cpu_seconds", "peak_rss_kib", "output_bytes")
        }
    RESULTS.mkdir(exist_ok=True)
    result_path = RESULTS / f"parallel-codegen-{dt.datetime.now().strftime('%Y%m%d-%H%M%S')}.json"
    result_path.write_text(
        json.dumps(
            {
                "experiment": "parallel lowering of independent reachable helper bodies",
                "measurement": "interleaved fresh-process build-subset runs; QUILL_JOBS and QUILL_TYPECK_JOBS fixed at host CPU count; QUILL_CODEGEN_JOBS varies; Linux wait4 wall, child user+system CPU, peak RSS; each output checked against rustc by exit code 42",
                "toolchain": subprocess.run(["rustc", "--version"], text=True, capture_output=True, check=True).stdout.strip(),
                "host": {"platform": platform.platform(), "logical_cpus": os.cpu_count()},
                "workload": {"name": "reachable-helper-chain", "helpers": args.helpers, "source_lines": source_lines, "source_sha256": source_sha256},
                "compiler_sha256": hashlib.sha256(COMPILER.read_bytes()).hexdigest(),
                "rounds": args.rounds,
                "rows": rows,
                "medians": summaries,
            },
            indent=2,
        )
        + "\n"
    )
    print(f"results: {result_path.relative_to(ROOT)}")
    print(json.dumps(summaries, indent=2))


if __name__ == "__main__":
    main()
