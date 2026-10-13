#!/usr/bin/env python3
"""Compare type-check method lookup before and after method-name indexing."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import resource
import statistics
import subprocess
import tempfile
import time


ROOT = pathlib.Path(__file__).resolve().parents[1]


def digest(path: pathlib.Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def generate_source(path: pathlib.Path, method_count: int) -> bytes:
    with path.open("w", encoding="utf-8") as source:
        source.write("struct Subject;\nimpl Subject {\n")
        for index in range(method_count):
            source.write(f"fn method_{index}(&self) -> u64 {{ {index} }}\n")
        source.write("}\nfn exercise(subject: &Subject) {\n")
        for index in range(method_count):
            source.write(f"let value_{index} = subject.method_{index}();\n")
        source.write("}\nfn main() {}\n")
    return path.read_bytes()


def run_one(
    binary: pathlib.Path,
    manifest: pathlib.Path,
    cache: pathlib.Path,
    expected_functions: int,
) -> dict[str, float | int]:
    environment = os.environ.copy()
    environment["QUILL_CACHE_DIR"] = str(cache)
    started = time.perf_counter()
    process = subprocess.Popen(
        [str(binary), "cargo-check-subset", str(manifest)],
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    waited_pid, status, usage = os.wait4(process.pid, 0)
    wall_seconds = time.perf_counter() - started
    stdout, stderr = process.communicate()
    exit_code = os.waitstatus_to_exitcode(status)
    if waited_pid != process.pid or exit_code != 0:
        raise RuntimeError(f"{binary} failed ({exit_code}): {stderr[-2000:]}")
    expected = f"checked {expected_functions} bodies"
    if expected not in stdout:
        raise RuntimeError(f"unexpected compiler summary: {stdout.strip()!r}")
    return {
        "wall_seconds": round(wall_seconds, 6),
        "cpu_seconds": round(usage.ru_utime + usage.ru_stime, 6),
        "peak_rss_mib": round(usage.ru_maxrss / 1024, 2),
    }


def summarize(rows: list[dict[str, float | int]]) -> dict[str, float]:
    return {
        key: round(statistics.median(float(row[key]) for row in rows), 6)
        for key in ("wall_seconds", "cpu_seconds", "peak_rss_mib")
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--before-bin", type=pathlib.Path, required=True)
    parser.add_argument("--after-bin", type=pathlib.Path, required=True)
    parser.add_argument("--before-revision", default="unspecified")
    parser.add_argument("--after-revision", default="unspecified")
    parser.add_argument("--method-count", type=int, default=3000)
    parser.add_argument("--rounds", type=int, default=7)
    parser.add_argument("--output", type=pathlib.Path)
    args = parser.parse_args()
    if args.method_count <= 0 or args.rounds <= 0:
        parser.error("--method-count and --rounds must be positive")
    for binary in (args.before_bin, args.after_bin):
        if not binary.is_file() or not os.access(binary, os.X_OK):
            parser.error(f"compiler binary is not executable: {binary}")

    with tempfile.TemporaryDirectory(prefix="quill-method-index-") as temporary:
        directory = pathlib.Path(temporary)
        source_path = directory / "src" / "main.rs"
        source_path.parent.mkdir()
        source_bytes = generate_source(source_path, args.method_count)
        manifest = directory / "Cargo.toml"
        manifest.write_text(
            '[package]\nname = "method-index-fixture"\nversion = "0.1.0"\nedition = "2024"\n',
            encoding="utf-8",
        )
        binaries = {
            "before": args.before_bin.resolve(),
            "after": args.after_bin.resolve(),
        }
        rows: list[dict[str, float | int | str]] = []
        for round_number in range(1, args.rounds + 1):
            order = ("before", "after") if round_number % 2 else ("after", "before")
            for variant in order:
                sample = run_one(
                    binaries[variant],
                    manifest,
                    directory / "cache" / f"{variant}-{round_number}",
                    args.method_count + 2,
                )
                rows.append({"variant": variant, "round": round_number, **sample})

        grouped = {
            variant: [row for row in rows if row["variant"] == variant]
            for variant in binaries
        }
        report = {
            "schema_version": 1,
            "experiment": "type-check inherent-method lookup indexing",
            "toolchain": subprocess.run(
                ["rustc", "+nightly-2026-08-23", "--version"],
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip(),
            "workload": {
                "method_count": args.method_count,
                "method_call_count": args.method_count,
                "function_count": args.method_count + 2,
                "source_lines": source_bytes.count(b"\n") + 1,
                "source_bytes": len(source_bytes),
                "source_sha256": hashlib.sha256(source_bytes).hexdigest(),
            },
            "methodology": "interleaved fresh-process runs with a fresh Quill query cache per sample; wall and CPU from wait4, root-process peak RSS from wait4",
            "variants": {
                "before": {
                    "revision": args.before_revision,
                    "binary_sha256": digest(binaries["before"]),
                    "median": summarize(grouped["before"]),
                },
                "after": {
                    "revision": args.after_revision,
                    "binary_sha256": digest(binaries["after"]),
                    "median": summarize(grouped["after"]),
                },
            },
            "rows": rows,
        }
        rendered = json.dumps(report, indent=2) + "\n"
        if args.output:
            args.output.write_text(rendered, encoding="utf-8")
        print(rendered, end="")


if __name__ == "__main__":
    main()
