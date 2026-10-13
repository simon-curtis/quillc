#!/usr/bin/env python3
"""Collect comparable cargo build timings for catalog-flow."""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import platform
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent
FIXTURE = ROOT / "catalog-flow"
RESULTS = ROOT / "results"
LEAF = FIXTURE / "catalog-audit" / "src" / "shard_30.rs"
CGBENCH_PREFIX = "QBENCH_ROW="


def output(command: list[str], cwd: Path | None = None) -> str:
    return subprocess.check_output(command, cwd=cwd, text=True, stderr=subprocess.STDOUT).strip()


def git_revision() -> str:
    try:
        return output(["git", "rev-parse", "HEAD"], ROOT.parent)
    except subprocess.CalledProcessError:
        return "uncommitted"


def cpu_model() -> str | None:
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.lower().startswith(("model name", "hardware")) and ":" in line:
                return line.split(":", 1)[1].strip()
    except OSError:
        pass
    return None


def cpu_governor() -> str | None:
    try:
        return Path("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor").read_text().strip()
    except OSError:
        return None


def cgroup_path() -> Path:
    relative = next(
        line.split("::", 1)[1].strip()
        for line in Path("/proc/self/cgroup").read_text().splitlines()
        if "::" in line
    )
    return Path("/sys/fs/cgroup") / relative.lstrip("/")


def cpu_counters(path: Path) -> dict[str, int]:
    return {
        key: int(value)
        for key, value in (line.split() for line in (path / "cpu.stat").read_text().splitlines())
        if key in {"usage_usec", "user_usec", "system_usec"}
    }


def worker(label: str, round_number: int) -> None:
    group = cgroup_path()
    peak_file = group / "memory.peak"
    if not peak_file.exists():
        raise SystemExit("cgroup v2 memory.peak is unavailable inside the systemd scope")
    peak_file.write_text("0")
    before_cpu = cpu_counters(group)
    env = os.environ.copy()
    env["CARGO_INCREMENTAL"] = "1"
    env.pop("CI", None)
    env.pop("RUSTC_WRAPPER", None)
    env.pop("RUSTC_WORKSPACE_WRAPPER", None)
    started = time.perf_counter()
    proc = subprocess.run(["cargo", "build", "--workspace", "--locked"], cwd=FIXTURE, env=env)
    elapsed = time.perf_counter() - started
    after_cpu = cpu_counters(group)
    row = {
        "round": round_number,
        "mode": label,
        "wall_seconds": elapsed,
        "cpu_user_seconds": (after_cpu.get("user_usec", 0) - before_cpu.get("user_usec", 0)) / 1_000_000,
        "cpu_system_seconds": (after_cpu.get("system_usec", 0) - before_cpu.get("system_usec", 0)) / 1_000_000,
        "cpu_total_seconds": (after_cpu["usage_usec"] - before_cpu["usage_usec"]) / 1_000_000,
        "peak_memory_bytes": int(peak_file.read_text().strip()),
        "exit_code": proc.returncode,
    }
    print(CGBENCH_PREFIX + json.dumps(row), flush=True)
    raise SystemExit(proc.returncode)


def one_build(label: str, round_number: int) -> dict[str, object]:
    unit = f"quillc-bench-{os.getpid()}-{round_number}-{label}-{time.time_ns()}"
    command = [
        "systemd-run", "--user", "--scope", "--quiet", f"--unit={unit}",
        "-p", "MemoryAccounting=yes", sys.executable, str(Path(__file__).resolve()),
        "--worker", label, str(round_number),
    ]
    proc = subprocess.run(command, cwd=FIXTURE, text=True, capture_output=True)
    rows = [line[len(CGBENCH_PREFIX):] for line in proc.stdout.splitlines() if line.startswith(CGBENCH_PREFIX)]
    if proc.returncode or not rows:
        sys.stderr.write(proc.stdout)
        sys.stderr.write(proc.stderr)
        raise SystemExit(f"{label} build measurement failed with exit status {proc.returncode}")
    row = json.loads(rows[-1])
    for line in proc.stdout.splitlines():
        if not line.startswith(CGBENCH_PREFIX):
            print(line)
    print(
        f"round {round_number} {label}: wall={row['wall_seconds']:.3f}s "
        f"cpu={row['cpu_total_seconds']:.2f}s peak={row['peak_memory_bytes'] / 1024:.0f} KiB",
        flush=True,
    )
    return row


def verify_binary() -> str:
    binary = FIXTURE / "target" / "debug" / "catalog-flow"
    result = output([str(binary)])
    expected = "catalog-flow records=128 digest="
    if not result.startswith(expected):
        raise SystemExit(f"unexpected benchmark program output: {result}")
    return result


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rounds", type=int, default=5)
    parser.add_argument("--worker", nargs=2, metavar=("MODE", "ROUND"))
    args = parser.parse_args()
    if args.worker:
        worker(args.worker[0], int(args.worker[1]))
        return
    if args.rounds < 1:
        parser.error("--rounds must be at least 1")

    # Write the lockfile before measurement so it is not counted as build work.
    subprocess.run(["cargo", "generate-lockfile"], cwd=FIXTURE, check=True)
    original = LEAF.read_bytes()
    rows: list[dict[str, object]] = []
    program_output: str | None = None
    try:
        for round_number in range(1, args.rounds + 1):
            shutil.rmtree(FIXTURE / "target", ignore_errors=True)
            rows.append(one_build("cold", round_number))
            if program_output is None:
                program_output = verify_binary()
            rows.append(one_build("warm", round_number))
            LEAF.write_bytes(original + f"\n// incremental probe round {round_number}\n".encode())
            rows.append(one_build("incremental", round_number))
            LEAF.write_bytes(original)
    finally:
        LEAF.write_bytes(original)

    stamp = dt.datetime.now().astimezone().isoformat(timespec="seconds")
    RESULTS.mkdir(exist_ok=True)
    result_path = RESULTS / f"baseline-{dt.datetime.now().strftime('%Y%m%d-%H%M%S')}.json"
    data = {
        "schema_version": 1,
        "timestamp_local": stamp,
        "git_revision": git_revision(),
        "host": {
            "platform": platform.platform(),
            "machine": platform.machine(),
            "cpu_model": cpu_model(),
            "cpu_governor": cpu_governor(),
            "logical_cpus": os.cpu_count(),
        },
        "rustc_version": output(["rustc", "--version", "--verbose"]),
        "cargo_version": output(["cargo", "--version"]),
        "source_lines": sum(len(path.read_text().splitlines()) for path in FIXTURE.rglob("*.rs")),
        "program_output": program_output,
        "metrics_note": "systemd scope cgroup v2 cpu.stat and memory.peak deltas; memory includes the lightweight Python measurement wrapper.",
        "runs": rows,
    }
    result_path.write_text(json.dumps(data, indent=2) + "\n")
    print(f"results: {result_path}")
    print(f"compiler: {data['rustc_version'].splitlines()[0]}")
    print(f"source lines: {data['source_lines']}")
    for mode in ("cold", "warm", "incremental"):
        selected = [row for row in rows if row["mode"] == mode]
        walls = sorted(float(row["wall_seconds"]) for row in selected)
        rss = sorted(int(row["peak_memory_bytes"]) for row in selected)
        median_wall = walls[len(walls) // 2] if len(walls) % 2 else sum(walls[len(walls)//2-1:len(walls)//2+1]) / 2
        median_rss = rss[len(rss) // 2] if len(rss) % 2 else (rss[len(rss)//2-1] + rss[len(rss)//2]) // 2
        print(f"{mode} median: wall={median_wall:.3f}s peak={median_rss / 1024:.0f} KiB")


if __name__ == "__main__":
    main()
