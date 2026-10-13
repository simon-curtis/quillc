#!/usr/bin/env python3
"""Compare executable codegen reachability on generated public dead helpers."""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import statistics
import subprocess
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent
RESULTS = ROOT / "results"
GENERATOR = ROOT / "generate_dead_code_pruning.py"


def measure(binary: Path, source: Path, output: Path, variant: str, round_number: int) -> dict[str, object]:
    started = time.perf_counter()
    process = subprocess.Popen(
        [str(binary), "build-subset", str(source), "-o", str(output)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    _, status, usage = os.wait4(process.pid, 0)
    wall = time.perf_counter() - started
    process.returncode = os.waitstatus_to_exitcode(status)
    stdout = process.stdout.read()
    stderr = process.stderr.read()
    if process.returncode:
        raise RuntimeError(f"{variant} round {round_number} failed:\n{stdout}\n{stderr}")
    run = subprocess.run([str(output)], check=False)
    if run.returncode != 42:
        raise RuntimeError(f"{variant} output returned {run.returncode}, expected 42")
    row = {
        "round": round_number,
        "variant": variant,
        "wall_seconds": wall,
        "cpu_seconds": usage.ru_utime + usage.ru_stime,
        "peak_rss_kib": usage.ru_maxrss,
        "output_bytes": output.stat().st_size,
        "program_exit_code": run.returncode,
    }
    print(
        f"round {round_number} {variant}: wall={wall:.3f}s "
        f"cpu={row['cpu_seconds']:.2f}s rss={usage.ru_maxrss} KiB "
        f"binary={row['output_bytes']} B",
        flush=True,
    )
    return row


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--before", type=Path, required=True, help="pre-change quillc executable")
    parser.add_argument("--after", type=Path, required=True, help="post-change quillc executable")
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--helpers", type=int, default=3400)
    args = parser.parse_args()
    if args.rounds < 1 or args.helpers < 1:
        parser.error("--rounds and --helpers must be positive")

    source = Path(f"/tmp/quill-public-dead-{os.getpid()}.rs")
    subprocess.run(
        ["python3", str(GENERATOR), str(source), "--helpers", str(args.helpers), "--public"],
        check=True,
    )
    source_lines = sum(1 for _ in source.open())
    source_sha256 = hashlib.sha256(source.read_bytes()).hexdigest()
    rows: list[dict[str, object]] = []
    try:
        for round_number in range(1, args.rounds + 1):
            order = (("before", args.before), ("after", args.after))
            if round_number % 2 == 0:
                order = tuple(reversed(order))
            for variant, binary in order:
                output = Path(f"/tmp/quill-public-dead-{os.getpid()}-{variant}-{round_number}")
                rows.append(measure(binary.resolve(), source, output, variant, round_number))
                output.unlink()
    finally:
        source.unlink(missing_ok=True)

    summaries = {}
    for variant in ("before", "after"):
        selected = [row for row in rows if row["variant"] == variant]
        summaries[variant] = {
            key: statistics.median(float(row[key]) for row in selected)
            for key in ("wall_seconds", "cpu_seconds", "peak_rss_kib", "output_bytes")
        }
    RESULTS.mkdir(exist_ok=True)
    stamp = dt.datetime.now().strftime("%Y%m%d-%H%M%S")
    result_path = RESULTS / f"dead-public-codegen-{stamp}.json"
    data = {
        "experiment": "closed executable public dead-code pruning",
        "rounds": args.rounds,
        "helpers": args.helpers,
        "source_lines": source_lines,
        "source_sha256": source_sha256,
        "measurement": "wait4 wall, CPU, and peak RSS for quillc plus waited linker child; generated program must exit 42",
        "binaries": {
            variant: {
                "path": str(binary.resolve()),
                "sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
            }
            for variant, binary in (("before", args.before), ("after", args.after))
        },
        "rows": rows,
        "medians": summaries,
    }
    result_path.write_text(json.dumps(data, indent=2) + "\n")
    print(f"results: {result_path}")
    print(json.dumps(summaries, indent=2))


if __name__ == "__main__":
    main()
