#!/usr/bin/env python3
"""Run differential Rust source cases against rustc and quillc."""

from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import os
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib
from dataclasses import asdict, dataclass


ROOT = pathlib.Path(__file__).resolve().parents[2]
CASES = pathlib.Path(__file__).with_name("cases.json")
TOOLCHAIN = tomllib.loads((ROOT / "rust-toolchain.toml").read_text(encoding="utf-8"))["toolchain"]["channel"]


@dataclass
class Result:
    case: str
    kind: str
    passed: bool
    rustc_accepted: bool
    quill_accepted: bool
    reason: str
    rustc_status: int | None = None
    quill_status: int | None = None
    runtime_match: bool | None = None
    diagnostic_checks: dict[str, bool] | None = None
    diagnostic_observed: dict | None = None


def invoke(argv: list[str], cwd: pathlib.Path) -> subprocess.CompletedProcess[bytes]:
    if argv and argv[0] in {"cargo", "rustc"} and not any(
        argument.startswith("+") for argument in argv[1:2]
    ):
        argv = [argv[0], f"+{TOOLCHAIN}", *argv[1:]]
    try:
        return subprocess.run(argv, cwd=cwd, capture_output=True, check=False)
    except OSError as error:
        raise RuntimeError(f"could not run {argv[0]!r}: {error}") from error


def resolve_quill() -> pathlib.Path:
    configured = os.environ.get("QUILLC")
    if configured:
        candidates = [pathlib.Path(configured)]
    else:
        candidates = [ROOT / "target/release/quillc", ROOT / "target/debug/quillc"]
        candidates = sorted(candidates, key=lambda path: path.stat().st_mtime if path.exists() else 0,
                           reverse=True)
    for path in candidates:
        resolved = path.expanduser().resolve()
        if resolved.is_file() and os.access(resolved, os.X_OK):
            return resolved
    raise RuntimeError("quillc executable not found; build it or set QUILLC")


def test_case(case: dict, quill: pathlib.Path, temp_root: pathlib.Path) -> Result:
    case_dir = temp_root / case["id"]
    case_dir.mkdir()
    rust_binary = case_dir / "rustc-program"
    quill_binary = case_dir / "quill-program"
    if case["kind"] == "diagnostic":
        source = case_dir / "main.rs"
        source.write_text(case["source"] + "\n", encoding="utf-8")
        rust_meta = case_dir / "rustc.rmeta"
        rustc_result = invoke(
            ["rustc", "--edition", case.get("edition", "2024"),
             "--crate-type", "lib", "--emit=metadata", "--error-format=json",
             str(source), "-o", str(rust_meta)], case_dir)
        quill_result = invoke([str(quill), "check-subset", str(source)], case_dir)
        rust_diagnostics = []
        for line in rustc_result.stderr.decode("utf-8", "replace").splitlines():
            try:
                diagnostic = json.loads(line)
            except json.JSONDecodeError:
                continue
            if diagnostic.get("level") == "error" and diagnostic.get("code"):
                rust_diagnostics.append(diagnostic)
        rust_primary = next((span for span in rust_diagnostics[0].get("spans", [])
                             if span.get("is_primary")), {}) if rust_diagnostics else {}
        quill_text = (quill_result.stdout + quill_result.stderr).decode("utf-8", "replace")
        quill_code = re.search(r"\bE\d{4}\b", quill_text)
        quill_location = re.search(r":(\d+):(\d+):", quill_text)
        checks = {
            "both_reject": rustc_result.returncode != 0 and quill_result.returncode != 0,
            "rustc_error_code_matches": bool(rust_diagnostics)
            and rust_diagnostics[0].get("code", {}).get("code") == case["rustc_error_code"],
            "quill_error_code_matches": bool(quill_code)
            and quill_code.group(0) == case["rustc_error_code"],
            "rustc_primary_message_matches": bool(rust_diagnostics)
            and rust_diagnostics[0].get("message") == case["rustc_message"],
            "quill_primary_message_matches": bool(rust_diagnostics)
            and rust_diagnostics[0].get("message") in quill_text,
            "primary_span_matches": bool(rust_primary and quill_location)
            and rust_primary.get("line_start") == int(quill_location.group(1))
            and rust_primary.get("column_start") == int(quill_location.group(2)),
        }
        observed = {
            "rustc_code": rust_diagnostics[0].get("code", {}).get("code") if rust_diagnostics else None,
            "rustc_message": rust_diagnostics[0].get("message") if rust_diagnostics else None,
            "rustc_primary_span": {
                "line": rust_primary.get("line_start"),
                "column": rust_primary.get("column_start"),
            } if rust_primary else None,
            "quill_code": quill_code.group(0) if quill_code else None,
            "quill_primary_location": {
                "line": int(quill_location.group(1)),
                "column": int(quill_location.group(2)),
            } if quill_location else None,
            "quill_diagnostic": next((line for line in quill_text.splitlines()
                                      if re.search(r":\d+:\d+:", line)), None),
        }
        passed = all(checks.values())
        failures = [key for key, ok in checks.items() if not ok]
        reason = "all diagnostic fields match" if passed else "diagnostic mismatch: " + ", ".join(failures)
        return Result(case["id"], case["kind"], passed,
                      rustc_result.returncode == 0, quill_result.returncode == 0,
                      reason, rustc_result.returncode, quill_result.returncode,
                      diagnostic_checks=checks, diagnostic_observed=observed)
    if case["kind"] == "abi-link":
        for relative_path, contents in case["files"].items():
            path = (case_dir / relative_path).resolve()
            if case_dir.resolve() not in path.parents:
                raise RuntimeError(f"case {case['id']} writes outside its directory")
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(contents, encoding="utf-8")
        lib_source = case_dir / case["library"]
        rust_main = case_dir / case["rust_main"]
        quill_main = case_dir / case["quill_main"]
        rust_lib_dir = case_dir / "rustc-lib"
        rust_lib_dir.mkdir()
        rust_lib = rust_lib_dir / "libprobe_lib.rlib"
        rustc_library = invoke(
            ["rustc", "--edition", "2024", "--crate-name", "probe_lib",
             "--crate-type", "rlib", str(lib_source), "--out-dir", str(rust_lib_dir)], case_dir)
        rustc_link = invoke(
            ["rustc", "--edition", "2024", str(rust_main), "--extern",
             f"probe_lib={rust_lib}", "-o", str(rust_binary)], case_dir)
        quill_object = case_dir / "quill-lib.o"
        quill_library = invoke(
            [str(quill), "build-lib-object", str(lib_source), "-o", str(quill_object)], case_dir)
        quill_link = invoke(
            ["rustc", "--edition", "2024", str(quill_main), "-C",
             f"link-arg={quill_object}", "-o", str(quill_binary)], case_dir)
        rust_ok = rustc_library.returncode == 0 and rustc_link.returncode == 0
        quill_ok = quill_library.returncode == 0 and quill_link.returncode == 0
        if not rust_ok:
            return Result(case["id"], case["kind"], False, False, quill_ok,
                          "rustc could not build its reference library program",
                          rustc_link.returncode, quill_link.returncode)
        if not quill_ok:
            detail = (quill_library.stderr + quill_link.stderr).decode("utf-8", "replace").strip()
            return Result(case["id"], case["kind"], False, True, False,
                          f"Quill object did not link into the rustc host: {detail}",
                          0, quill_link.returncode)
        rust_run = invoke([str(rust_binary)], case_dir)
        quill_run = invoke([str(quill_binary)], case_dir)
        same = (rust_run.returncode == quill_run.returncode
                and rust_run.stdout == quill_run.stdout
                and rust_run.stderr == quill_run.stderr)
        reason = "object interop matches" if same else "object interop behavior differs"
        return Result(case["id"], case["kind"], same, True, True, reason,
                      rustc_link.returncode, quill_link.returncode, same)
    if case["kind"] == "extern-artifact":
        for relative_path, contents in case["files"].items():
            path = (case_dir / relative_path).resolve()
            if case_dir.resolve() not in path.parents:
                raise RuntimeError(f"case {case['id']} writes outside its directory")
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(contents, encoding="utf-8")
        lib_source = case_dir / case["library"]
        main_source = case_dir / case["main"]
        rust_lib_dir = case_dir / "rustc-lib"
        rust_lib_dir.mkdir()
        rust_lib = rust_lib_dir / "libprobe_lib.rlib"
        rustc_library = invoke(
            ["rustc", "--edition", "2024", "--crate-name", "probe_lib",
             "--crate-type", "rlib", str(lib_source), "--out-dir", str(rust_lib_dir)], case_dir)
        rustc_link = invoke(
            ["rustc", "--edition", "2024", str(main_source), "--extern",
             f"probe_lib={rust_lib}", "-o", str(rust_binary)], case_dir)
        quill_object = case_dir / "libprobe_lib.o"
        quill_library = invoke(
            [str(quill), "build-lib-object", str(lib_source), "-o", str(quill_object)], case_dir)
        quill_link = invoke(
            ["rustc", "--edition", "2024", str(main_source), "--extern",
             f"probe_lib={quill_object}", "-o", str(quill_binary)], case_dir)
        if rustc_library.returncode or rustc_link.returncode:
            return Result(case["id"], case["kind"], False, False, False,
                          "rustc reference rlib case failed", rustc_link.returncode,
                          quill_link.returncode)
        if quill_library.returncode or quill_link.returncode:
            detail = (quill_library.stderr + quill_link.stderr).decode("utf-8", "replace").strip()
            return Result(case["id"], case["kind"], False, True, False,
                          f"Quill artifact is not consumable through rustc --extern: {detail}",
                          rustc_link.returncode, quill_link.returncode)
        rust_run = invoke([str(rust_binary)], case_dir)
        quill_run = invoke([str(quill_binary)], case_dir)
        same = (rust_run.returncode == quill_run.returncode
                and rust_run.stdout == quill_run.stdout
                and rust_run.stderr == quill_run.stderr)
        return Result(case["id"], case["kind"], same, True, True,
                      "extern artifact behavior matches" if same else "extern artifact behavior differs",
                      rustc_link.returncode, quill_link.returncode, same)
    if case["kind"] == "cargo-run":
        for relative_path, contents in case["files"].items():
            path = (case_dir / relative_path).resolve()
            if case_dir.resolve() not in path.parents:
                raise RuntimeError(f"case {case['id']} writes outside its directory")
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(contents, encoding="utf-8")
        manifest = (case_dir / case["manifest"]).resolve()
        if case_dir.resolve() not in manifest.parents:
            raise RuntimeError(f"case {case['id']} manifest is outside its directory")
        rust_target = case_dir / "rust-target"
        rustc_result = invoke(
            ["cargo", "build", "--quiet", "--manifest-path", str(manifest),
             "--target-dir", str(rust_target)], case_dir)
        if rustc_result.returncode:
            return Result(case["id"], case["kind"], False, False, False,
                          "rustc Cargo build failed", rustc_result.returncode)
        binary_names = case.get("binaries", [case["binary"]])
        quill_output_root = case_dir / "quill-target"
        if "binaries" in case:
            quill_result = invoke(
                [str(quill), "cargo-build-subset", str(manifest), "-o",
                 str(quill_output_root)], case_dir)
            if quill_result.returncode:
                detail = quill_result.stderr.decode("utf-8", "replace").strip()
                return Result(case["id"], case["kind"], False, True, False,
                              f"Quill rejected Cargo binary targets: {detail}",
                              rustc_result.returncode, quill_result.returncode)
        mismatches = []
        quill_status = 0 if "binaries" in case else None
        for binary_name in binary_names:
            rust_binary = rust_target / "debug" / binary_name
            if "binaries" in case:
                quill_binary = quill_output_root / binary_name
            else:
                quill_binary = case_dir / f"quill-{binary_name}"
                quill_result = invoke(
                    [str(quill), "cargo-build-subset", str(manifest), "-o",
                     str(quill_binary)], case_dir)
                quill_status = quill_result.returncode
                if quill_status:
                    detail = quill_result.stderr.decode("utf-8", "replace").strip()
                    return Result(case["id"], case["kind"], False, True, False,
                                  f"Quill rejected binary `{binary_name}`: {detail}",
                                  rustc_result.returncode, quill_status)
            rust_run = invoke([str(rust_binary)], case_dir)
            quill_run = invoke([str(quill_binary)], case_dir)
            if (rust_run.returncode != quill_run.returncode
                    or rust_run.stdout != quill_run.stdout
                    or rust_run.stderr != quill_run.stderr):
                mismatches.append(binary_name)
        passed = not mismatches
        reason = ("all Cargo binary outputs match" if passed else
                  "runtime status or output differs for: " + ", ".join(mismatches))
        return Result(case["id"], case["kind"], passed, True, True, reason,
                      rustc_result.returncode, quill_status, passed)
    elif case["kind"] == "parse":
        source = case_dir / "main.rs"
        source.write_text(case["source"] + "\n", encoding="utf-8")
        rust_meta = case_dir / "rustc.rmeta"
        rustc_result = invoke(
            ["rustc", "--edition", case.get("edition", "2024"),
             "--crate-type", "lib", "--emit=metadata", str(source), "-o", str(rust_meta)],
            case_dir)
        quill_args = [str(quill), "parse", str(source)]
    else:
        source = case_dir / "main.rs"
        source.write_text(case["source"] + "\n", encoding="utf-8")
        edition = case.get("edition", "2024")
        rustc_args = ["rustc", "--edition", edition, *case.get("rustc_args", [])]
        rustc_args += [str(source), "-o", str(rust_binary)]
        rustc_result = invoke(rustc_args, case_dir)
    rustc_accepted = rustc_result.returncode == 0
    if not rustc_accepted:
        return Result(case["id"], case["kind"], False, False, False,
                      "rustc rejected a positive corpus case", rustc_result.returncode)

    if case["kind"] == "run":
        quill_args = [str(quill), "build-subset", str(source), "-o", str(quill_binary)]
    quill_result = invoke(quill_args, case_dir)
    quill_accepted = quill_result.returncode == 0
    if case["kind"] == "parse":
        passed = rustc_accepted and quill_accepted
        reason = "both accept source syntax" if passed else "parser acceptance differs"
        return Result(case["id"], case["kind"], passed, rustc_accepted,
                      quill_accepted, reason, rustc_result.returncode,
                      quill_result.returncode, passed)
    if not quill_accepted:
        detail = quill_result.stderr.decode("utf-8", "replace").strip()
        return Result(case["id"], case["kind"], False, rustc_accepted, False,
                      f"Quill rejected positive case: {detail}",
                      rustc_result.returncode, quill_result.returncode)

    rust_run = invoke([str(rust_binary)], case_dir)
    quill_run = invoke([str(quill_binary)], case_dir)
    same = (rust_run.returncode == quill_run.returncode
            and rust_run.stdout == quill_run.stdout
            and rust_run.stderr == quill_run.stderr)
    reason = "runtime outputs match" if same else "runtime status or output differs"
    return Result(case["id"], case["kind"], same, rustc_accepted,
                  quill_accepted, reason, rustc_result.returncode,
                  quill_result.returncode, same)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--keep", action="store_true", help="keep generated source and binaries")
    parser.add_argument("--json", action="store_true", help="print a machine-readable report")
    parser.add_argument("--output", type=pathlib.Path,
                        help="write the machine-readable report to this path")
    parser.add_argument("--case", action="append", help="run only this case id; repeatable")
    args = parser.parse_args()

    try:
        quill = resolve_quill()
        version = invoke(["rustc", "--version", "--verbose"], ROOT)
        if version.returncode:
            raise RuntimeError("rustc --version --verbose failed")
        revision = invoke(["git", "rev-parse", "HEAD"], ROOT)
        if revision.returncode:
            raise RuntimeError("could not read Quill source revision")
        compiler_paths = [
            "crates/quill-codegen/src/lib.rs",
            "crates/quill-syntax/src/lib.rs",
            "crates/quill-typeck/src/lib.rs",
            "crates/quillc/src/main.rs",
            "crates/quill-cargo/src/lib.rs",
        ]
        dirty_sources = invoke(["git", "status", "--porcelain", "--", *compiler_paths], ROOT)
        if dirty_sources.returncode:
            raise RuntimeError("could not inspect Quill source status")
        cases = json.loads(CASES.read_text(encoding="utf-8"))
        if args.case:
            selected = set(args.case)
            unknown = selected - {case["id"] for case in cases}
            if unknown:
                raise RuntimeError(f"unknown case id(s): {', '.join(sorted(unknown))}")
            cases = [case for case in cases if case["id"] in selected]
        if not cases:
            raise RuntimeError("no compatibility cases selected")
        if len({case["id"] for case in cases}) != len(cases):
            raise RuntimeError("case ids must be unique")
        if any(case.get("kind") not in {"run", "cargo-run", "parse", "diagnostic", "abi-link", "extern-artifact"} for case in cases):
            raise RuntimeError("unsupported case kind")
        tracker_html = (ROOT / "compatibility.html").read_text(encoding="utf-8")
        feature_rows = re.findall(
            r'<section class="compatibility-row" data-status="([^"]+)" id="([^"]+)"',
            tracker_html,
        )
        tracked_features = {feature_id: status for status, feature_id in feature_rows}
        unknown_features = {
            feature for case in cases for feature in case.get("features", [])
            if feature not in tracked_features
        }
        if unknown_features:
            raise RuntimeError(f"unknown tracker feature id(s): {', '.join(sorted(unknown_features))}")

        temp = tempfile.TemporaryDirectory(prefix="quill-rustc-compat-")
        temp_root = pathlib.Path(temp.name)
        results = [test_case(case, quill, temp_root) for case in cases]
        passed = sum(result.passed for result in results)
        by_case = {result.case: result for result in results}
        feature_coverage = []
        for feature in sorted(tracked_features):
            probes = [case for case in cases if feature in case.get("features", [])]
            feature_coverage.append({
                "id": feature,
                "tracker_status": tracked_features[feature],
                "probed": bool(probes),
                "cases": len(probes),
                "passed": sum(by_case[case["id"]].passed for case in probes),
                "failed": [case["id"] for case in probes if not by_case[case["id"]].passed],
            })
        report = {
            "toolchain": TOOLCHAIN,
            "rustc": version.stdout.decode("utf-8", "replace").strip(),
            "quillc": str(quill),
            "quillc_source_revision": revision.stdout.decode().strip(),
            "quillc_source_dirty": bool(dirty_sources.stdout.strip()),
            "quillc_artifact_mtime": datetime.datetime.fromtimestamp(
                quill.stat().st_mtime, datetime.timezone.utc).isoformat(),
            "quillc_sha256": hashlib.sha256(quill.read_bytes()).hexdigest(),
            "passed": passed,
            "total": len(results),
            "percent": round(100 * passed / len(results), 1),
            "results": [asdict(result) for result in results],
            "feature_coverage": feature_coverage,
        }
        if args.json:
            print(json.dumps(report, indent=2))
        else:
            print(report["rustc"])
            print(f"Quill: {quill}")
            for result in results:
                marker = "PASS" if result.passed else "FAIL"
                print(f"{marker} {result.case}: {result.reason}")
            print(f"Compatibility corpus: {passed}/{len(results)} ({report['percent']:.1f}%)")
        if args.output:
            output = args.output.expanduser()
            if not output.is_absolute():
                output = ROOT / output
            output.parent.mkdir(parents=True, exist_ok=True)
            output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
            if not args.json:
                print(f"Report: {output}")
        if args.keep:
            kept = pathlib.Path(temp.name).with_name(pathlib.Path(temp.name).name + "-kept")
            shutil.move(temp.name, kept)
            temp._finalizer.detach()
            print(f"Artifacts: {kept}", file=sys.stderr)
        temp.cleanup()
        return 0 if passed == len(results) else 1
    except (OSError, RuntimeError, KeyError, json.JSONDecodeError) as error:
        print(f"compatibility runner: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
