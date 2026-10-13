#!/usr/bin/env python3
"""Measure rustc Cargo builds and Quill's native Cargo subset build."""

from __future__ import annotations

import hashlib
import json
import os
import pathlib
import resource
import shutil
import statistics
import subprocess
import threading
import tempfile
import time

import psutil


ROOT = pathlib.Path(__file__).resolve().parents[2]
BENCH = ROOT / "benchmarks/catalog-flow"
MANIFEST = BENCH / "Cargo.toml"
SOURCE = BENCH / "catalog-validate/src/lib.rs"
SEMANTIC_SOURCE = BENCH / "catalog-flow/src/lib.rs"
RUST_TARGET = pathlib.Path(tempfile.gettempdir()) / "catalog-flow-rustc-measure"
QUILL_OUTPUT = pathlib.Path(tempfile.gettempdir()) / "catalog-flow-quill-measure"
QUILL_CACHE = pathlib.Path(tempfile.gettempdir()) / "catalog-flow-quill-query-cache"
QUILL = ROOT / "target/release/quillc"
TOOLCHAIN = "+nightly-2026-08-23"


def measure(
    label: str, command: list[str], env: dict[str, str] | None = None
) -> dict[str, float | int | str]:
    before_children = resource.getrusage(resource.RUSAGE_CHILDREN)
    started = time.perf_counter()
    process = subprocess.Popen(
        command,
        cwd=ROOT,
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    root_process = psutil.Process(process.pid)
    peak_rss = 0

    def sample_process_tree() -> None:
        nonlocal peak_rss
        try:
            tree = [root_process, *root_process.children(recursive=True)]
            peak_rss = max(
                peak_rss,
                sum(child.memory_info().rss for child in tree if child.is_running()),
            )
        except psutil.Error:
            pass

    sample_process_tree()
    stop_sampling = threading.Event()

    def sample_until_stopped() -> None:
        while not stop_sampling.wait(0.002):
            sample_process_tree()

    sampler = threading.Thread(target=sample_until_stopped, daemon=True)
    sampler.start()
    waited_pid, status, process_usage = os.wait4(process.pid, 0)
    finished = time.perf_counter()
    stop_sampling.set()
    sampler.join()
    if waited_pid != process.pid:
        raise RuntimeError(f"{label}: wait4 returned unexpected pid {waited_pid}")
    process.returncode = os.waitstatus_to_exitcode(status)
    peak_rss = max(peak_rss, process_usage.ru_maxrss * 1024)
    return_code = process.returncode
    wall = finished - started
    after_children = resource.getrusage(resource.RUSAGE_CHILDREN)
    user = after_children.ru_utime - before_children.ru_utime
    system = after_children.ru_stime - before_children.ru_stime
    if return_code:
        raise RuntimeError(f"{label} failed ({return_code}): {command}")
    return {
        "label": label,
        "wall_seconds": round(wall, 3),
        "user_seconds": round(user, 3),
        "system_seconds": round(system, 3),
        "cpu_seconds": round(user + system, 3),
        "peak_process_tree_rss_mib": round(peak_rss / (1024 * 1024), 1),
        "command": " ".join(command),
    }


def main() -> None:
    if not QUILL.is_file():
        raise SystemExit(
            "Build Quill first: cargo +nightly-2026-08-23 build --release -p quillc"
        )
    results = []
    rustc_command = [
        "cargo",
        TOOLCHAIN,
        "build",
        "--quiet",
        "--package",
        "catalog-flow",
        "--manifest-path",
        str(MANIFEST),
        "--target-dir",
        str(RUST_TARGET),
    ]
    quill_command = [
        str(QUILL),
        "cargo-build-subset",
        str(MANIFEST),
        "-o",
        str(QUILL_OUTPUT),
    ]
    quill_env = os.environ.copy()
    quill_env["QUILL_CACHE_DIR"] = str(QUILL_CACHE)

    backup = SOURCE.read_bytes()
    semantic_backup = SEMANTIC_SOURCE.read_bytes()
    semantic_marker = b"1..=128"
    if semantic_backup.count(semantic_marker) != 1:
        raise SystemExit("semantic benchmark marker must appear exactly once")
    semantic_changed = semantic_backup.replace(semantic_marker, b"1..=127")
    changed_runtime_output = None
    try:
        for run in range(1, 6):
            shutil.rmtree(RUST_TARGET, ignore_errors=True)
            shutil.rmtree(QUILL_CACHE, ignore_errors=True)
            QUILL_OUTPUT.unlink(missing_ok=True)
            for label, command, env in [
                ("rustc_cold", rustc_command, None),
                ("rustc_warm", rustc_command, None),
                ("quill_cold", quill_command, quill_env),
                ("quill_warm", quill_command, quill_env),
            ]:
                result = measure(label, command, env)
                result["run"] = run
                results.append(result)
            SOURCE.write_bytes(backup + b"\n// incremental measurement marker\n")
            for label, command, env in [
                ("rustc_incremental", rustc_command, None),
                ("quill_incremental", quill_command, quill_env),
            ]:
                result = measure(label, command, env)
                result["run"] = run
                results.append(result)
            SOURCE.write_bytes(backup)

            SEMANTIC_SOURCE.write_bytes(semantic_changed)
            for label, command, env in [
                ("rustc_semantic_incremental", rustc_command, None),
                ("quill_semantic_incremental", quill_command, quill_env),
            ]:
                result = measure(label, command, env)
                result["run"] = run
                results.append(result)
            rustc_changed = subprocess.run(
                [str(RUST_TARGET / "debug/catalog-flow")],
                capture_output=True,
                text=True,
                check=True,
            ).stdout
            quill_changed = subprocess.run(
                [str(QUILL_OUTPUT)], capture_output=True, text=True, check=True
            ).stdout
            if rustc_changed != quill_changed:
                raise SystemExit(
                    "semantic incremental runtime outputs differ:\n"
                    f"rustc: {rustc_changed!r}\nQuill: {quill_changed!r}"
                )
            if changed_runtime_output is not None and changed_runtime_output != rustc_changed:
                raise SystemExit("semantic edit produced inconsistent output across runs")
            changed_runtime_output = rustc_changed
            SEMANTIC_SOURCE.write_bytes(semantic_backup)
    finally:
        SOURCE.write_bytes(backup)
        SEMANTIC_SOURCE.write_bytes(semantic_backup)

    # Restore the original semantics in each compiler's final runnable artifact.
    subprocess.run(rustc_command, cwd=ROOT, check=True, stdout=subprocess.DEVNULL)
    subprocess.run(
        quill_command, cwd=ROOT, env=quill_env, check=True, stdout=subprocess.DEVNULL
    )

    rustc_binary = RUST_TARGET / "debug/catalog-flow"
    rustc_run = subprocess.run(
        [str(rustc_binary)], capture_output=True, text=True, check=True
    )
    quill_run = subprocess.run(
        [str(QUILL_OUTPUT)], capture_output=True, text=True, check=True
    )
    if rustc_run.stdout != quill_run.stdout:
        raise SystemExit(
            f"runtime outputs differ:\nrustc: {rustc_run.stdout!r}\n"
            f"Quill: {quill_run.stdout!r}"
        )
    if changed_runtime_output == rustc_run.stdout:
        raise SystemExit("semantic incremental edit did not change runtime behavior")

    compiler_paths = [
        ROOT / "Cargo.toml",
        ROOT / "Cargo.lock",
        ROOT / "rust-toolchain.toml",
    ]
    compiler_paths.extend(ROOT.glob("crates/*/Cargo.toml"))
    compiler_paths.extend(ROOT.glob("crates/*/src/**/*.rs"))
    compiler_paths = sorted(set(compiler_paths))
    compiler_status = subprocess.check_output(
        [
            "git",
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--",
            *[str(path.relative_to(ROOT)) for path in compiler_paths],
        ],
        cwd=ROOT,
        text=True,
    )
    compiler_dirty_paths = [line[3:] for line in compiler_status.splitlines()]
    data = {
        "toolchain": subprocess.check_output(
            ["cargo", TOOLCHAIN, "--version"], text=True
        ).strip(),
        "cpu_model": next(
            (
                line.split(":", 1)[1].strip()
                for line in pathlib.Path("/proc/cpuinfo").read_text().splitlines()
                if line.startswith("model name")
            ),
            "unknown",
        ),
        "logical_cpu_count": psutil.cpu_count(logical=True),
        "quill_profile": "release",
        "quillc_source_revision": subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True
        ).strip(),
        "quillc_source_dirty": bool(compiler_dirty_paths),
        "quillc_source_dirty_paths": compiler_dirty_paths,
        "quillc_sha256": hashlib.sha256(QUILL.read_bytes()).hexdigest(),
        "rust_lines": sum(
            len(path.read_text().splitlines())
            for path in BENCH.glob("catalog-*/src/**/*.rs")
        ),
        "runtime_output": rustc_run.stdout.strip(),
        "semantic_edit_runtime_output": changed_runtime_output.strip(),
        "memory_metric": "peak sampled RSS summed over each build process tree",
        "results": results,
        "medians": {
            label: {
                metric: round(
                    statistics.median(
                        result[metric]
                        for result in results
                        if result["label"] == label
                    ),
                    3,
                )
                for metric in [
                    "wall_seconds",
                    "cpu_seconds",
                    "peak_process_tree_rss_mib",
                ]
            }
            for label in [
                "rustc_cold",
                "quill_cold",
                "rustc_warm",
                "quill_warm",
                "rustc_incremental",
                "quill_incremental",
                "rustc_semantic_incremental",
                "quill_semantic_incremental",
            ]
        },
    }
    output = BENCH / "results.json"
    output.write_text(json.dumps(data, indent=2) + "\n")
    print(output.relative_to(ROOT))
    for label, result in data["medians"].items():
        print(
            f"{label}: wall={result['wall_seconds']}s "
            f"cpu={result['cpu_seconds']}s "
            f"rss={result['peak_process_tree_rss_mib']} MiB"
        )
    print(f"runtime output matches: {rustc_run.stdout.strip()}")


if __name__ == "__main__":
    main()
