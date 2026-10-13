#!/usr/bin/env python3
"""Measure Quill's persistent query caches on the catalog-flow fixture."""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import platform
import shutil
import statistics
import subprocess
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parent
FIXTURE = ROOT / "catalog-flow"
RESULTS = ROOT / "results"
LEAF = FIXTURE / "catalog-audit" / "src" / "shard_30.rs"


def output(command: list[str]) -> str:
    return subprocess.check_output(command, cwd=REPO, text=True, stderr=subprocess.STDOUT).strip()


def cpu_model() -> str | None:
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.lower().startswith(("model name", "hardware")) and ":" in line:
                return line.split(":", 1)[1].strip()
    except OSError:
        pass
    return None


def median(rows: list[dict[str, object]], key: str) -> float:
    return float(statistics.median(float(row[key]) for row in rows))


def measure(
    command: list[str], env: dict[str, str], mode: str, round_number: int, expected: str,
    check_typeck: bool,
) -> dict[str, object]:
    started = time.perf_counter()
    process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env)
    _, status, usage = os.wait4(process.pid, 0)
    wall = time.perf_counter() - started
    exit_code = os.waitstatus_to_exitcode(status)
    process.returncode = exit_code
    stdout = process.stdout.read()
    stderr = process.stderr.read()
    if exit_code != 0:
        raise RuntimeError(f"{mode} round {round_number} failed ({exit_code}):\n{stdout}\n{stderr}")
    if expected not in stdout:
        raise RuntimeError(f"{mode} round {round_number} did not check the expected corpus: {stdout}")
    def cache_stats(prefix: str) -> dict[str, int]:
        stats_line = next((line for line in stderr.splitlines() if line.startswith(prefix)), None)
        if stats_line is None:
            raise RuntimeError(f"{mode} round {round_number} did not report {prefix} statistics")
        return {
            key: int(value)
            for key, value in (field.split("=", 1) for field in stats_line.removeprefix(prefix).split())
        }

    parse_cache = cache_stats("quill-query: ")
    type_check_cache = cache_stats("quill-typeck-query: ") if check_typeck else None
    row = {
        "round": round_number,
        "mode": mode,
        "wall_seconds": wall,
        "cpu_seconds": usage.ru_utime + usage.ru_stime,
        "peak_rss_kib": usage.ru_maxrss,
        "exit_code": exit_code,
        "cache": {
            "parse": {
                "hits": parse_cache["hits"],
                "misses": parse_cache["misses"],
                "invalid_entries": parse_cache["invalid"],
                "write_failures": parse_cache["write_failures"],
            },
            **({
                "type_check": {
                    "hits": type_check_cache["hits"],
                    "misses": type_check_cache["misses"],
                    "invalid_entries": type_check_cache["invalid"],
                    "write_failures": type_check_cache["write_failures"],
                }
            } if type_check_cache else {}),
        },
    }
    print(
        f"round {round_number} {mode}: wall={wall:.3f}s "
        f"cpu={row['cpu_seconds']:.2f}s peak={usage.ru_maxrss} KiB "
        f"parse={parse_cache['hits']}h/{parse_cache['misses']}m",
        *([f"typeck={type_check_cache['hits']}h/{type_check_cache['misses']}m"] if type_check_cache else []),
        flush=True,
    )
    return row


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rounds", type=int, default=5)
    parser.add_argument(
        "--command",
        choices=("check-subset", "cargo-parse", "cargo-check-subset"),
        default="check-subset",
    )
    args = parser.parse_args()
    if args.rounds < 1:
        parser.error("--rounds must be at least 1")

    subprocess.run(["cargo", "build", "--locked", "-p", "quillc"], cwd=REPO, check=True)
    roots = sorted(FIXTURE.glob("*/src/lib.rs"))
    roots.append(FIXTURE / "catalog-flow" / "src" / "main.rs")
    binary = str(REPO / "target" / "debug" / "quillc")
    if args.command == "check-subset":
        command = [binary, "check-subset", *map(str, roots)]
        expected = "checked 3454 function bodies"
        check_typeck = True
    elif args.command == "cargo-check-subset":
        command = [binary, "cargo-check-subset", str(FIXTURE / "Cargo.toml")]
        expected = "checked 3454 bodies with the primitive checker"
        check_typeck = True
    else:
        command = [binary, "cargo-parse", str(FIXTURE / "Cargo.toml")]
        expected = "parsed 9 Cargo target roots into 50 source files"
        check_typeck = False
    original = LEAF.read_bytes()
    rows: list[dict[str, object]] = []
    try:
        with tempfile.TemporaryDirectory(prefix="quill-query-cache-") as temporary:
            cache_dir = Path(temporary) / "cache"
            env = os.environ.copy()
            env["QUILL_CACHE_DIR"] = str(cache_dir)
            env["QUILL_CACHE_STATS"] = "1"
            for round_number in range(1, args.rounds + 1):
                shutil.rmtree(cache_dir, ignore_errors=True)
                rows.append(measure(command, env, "cold", round_number, expected, check_typeck))
                rows.append(measure(command, env, "warm", round_number, expected, check_typeck))
                marker = b"state.wrapping_mul(33)"
                if original.count(marker) != 1:
                    raise RuntimeError(f"expected one incremental edit marker in {LEAF}")
                edited = original.replace(marker, b"state.wrapping_mul(34)", 1)
                LEAF.write_bytes(edited)
                rows.append(measure(command, env, "incremental", round_number, expected, check_typeck))
                LEAF.write_bytes(original)
    finally:
        LEAF.write_bytes(original)

    source_files = [path for path in FIXTURE.rglob("*.rs") if "target" not in path.parts]
    data = {
        "schema_version": 1,
        "timestamp_local": dt.datetime.now().astimezone().isoformat(timespec="seconds"),
        "toolchain": {
            "rustc": output(["rustc", "--version", "--verbose"]),
            "cargo": output(["cargo", "--version"]),
            "toolchain_file": (REPO / "rust-toolchain.toml").read_text().strip(),
        },
        "host": {
            "platform": platform.platform(),
            "cpu_model": cpu_model(),
            "logical_cpus": os.cpu_count(),
        },
        "workload": {
            "name": "catalog-flow",
            "source_lines": sum(len(path.read_text().splitlines()) for path in source_files),
            "source_files": len(source_files),
            "crate_roots": len(roots),
            "command": args.command,
            **({"function_bodies_checked": 3454} if check_typeck else {}),
        },
        "measurement_note": f"Quill {args.command} query-cache profile, not a Cargo build or rustc comparison. Process metrics use Linux wait4 rusage.",
        "runs": rows,
        "medians": {
            mode: {
                "wall_seconds": median(selected, "wall_seconds"),
                "cpu_seconds": median(selected, "cpu_seconds"),
                "peak_rss_kib": median(selected, "peak_rss_kib"),
            }
            for mode in ("cold", "warm", "incremental")
            for selected in [[row for row in rows if row["mode"] == mode]]
        },
    }
    RESULTS.mkdir(exist_ok=True)
    result = RESULTS / f"quill-{args.command}-cache-{dt.datetime.now().strftime('%Y%m%d-%H%M%S')}.json"
    result.write_text(json.dumps(data, indent=2) + "\n")
    print(f"results: {result}")
    for mode, values in data["medians"].items():
        print(
            f"{mode} median: wall={values['wall_seconds']:.3f}s "
            f"cpu={values['cpu_seconds']:.3f}s peak={values['peak_rss_kib']:.0f} KiB"
        )


if __name__ == "__main__":
    main()
