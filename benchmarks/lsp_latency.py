#!/usr/bin/env python3
"""Measure end-to-end LSP syntax, incremental edit, outline, and completion latency."""

from __future__ import annotations

import argparse
import json
import math
import platform
import statistics
import subprocess
import tempfile
import time
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


def corpus(module_count: int, functions_per_module: int) -> str:
    lines = ["#![allow(dead_code)]", "macro_rules! identity { ($e:expr) => { $e }; }"]
    for module in range(module_count):
        lines.extend([f"pub mod module_{module} {{", "    pub trait Convert<T> { fn convert(&self) -> T; }"])
        for function in range(functions_per_module):
            name = f"function_{module}_{function}"
            lines.extend([
                f"    pub async fn {name}(input: &[u8]) -> usize {{",
                "        let mut total = 0usize;",
                "        for byte in input.iter().copied() {",
                "            total += identity!(usize::from(byte));",
                "        }",
                "        total",
            ])
            if module == module_count - 1 and function == functions_per_module - 1:
                lines.append("        // EDIT000000")
            lines.append("    }")
        lines.extend(["}", ""])
    return "\n".join(lines)


class LspProcess:
    def __init__(self, executable: Path):
        self.process = subprocess.Popen(
            [str(executable), "lsp"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.next_id = 1
        self.pending_messages: list[dict] = []

    def send(self, message: dict) -> None:
        assert self.process.stdin is not None
        payload = json.dumps(message, separators=(",", ":")).encode()
        self.process.stdin.write(f"Content-Length: {len(payload)}\r\n\r\n".encode())
        self.process.stdin.write(payload)
        self.process.stdin.flush()

    def receive(self) -> dict:
        assert self.process.stdout is not None
        length = None
        while True:
            header = self.process.stdout.readline()
            if not header:
                stderr = self.process.stderr.read().decode(errors="replace") if self.process.stderr else ""
                raise RuntimeError(f"language server exited before replying: {stderr}")
            if header in (b"\r\n", b"\n"):
                break
            name, _, value = header.partition(b":")
            if name.lower() == b"content-length":
                length = int(value.strip())
        if length is None:
            raise RuntimeError("language server response has no Content-Length")
        payload = self.process.stdout.read(length)
        return json.loads(payload)

    def request(self, method: str, params: dict) -> tuple[dict, float]:
        request_id = self.next_id
        self.next_id += 1
        started = time.perf_counter_ns()
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        while True:
            response = self.receive()
            if response.get("id") == request_id:
                return response, (time.perf_counter_ns() - started) / 1_000_000
            self.pending_messages.append(response)

    def next_message(self) -> dict:
        if self.pending_messages:
            return self.pending_messages.pop(0)
        return self.receive()

    def notification(self, method: str, params: dict) -> None:
        self.send({"jsonrpc": "2.0", "method": method, "params": params})

    def shutdown(self) -> None:
        try:
            self.request("shutdown", {})
            self.notification("exit", {})
            self.process.wait(timeout=5)
        finally:
            if self.process.poll() is None:
                self.process.kill()
                self.process.wait()


def measure(args: argparse.Namespace) -> dict:
    source = corpus(args.modules, args.functions_per_module)
    with tempfile.TemporaryDirectory(prefix="quill-lsp-bench-") as directory:
        workspace = Path(directory)
        (workspace / "Cargo.toml").write_text(
            '[package]\nname = "lsp-benchmark"\nversion = "0.1.0"\nedition = "2024"\n'
        )
        source_path = workspace / "src" / "main.rs"
        source_path.parent.mkdir()
        source_path.write_text(source)
        uri = source_path.as_uri()
        server = LspProcess(args.quillc.resolve())
        samples: dict[str, list[float]] = {
            "initialize": [],
            "open_to_diagnostics": [],
            "incremental_edit_to_diagnostics": [],
            "burst_edits_to_latest_diagnostics": [],
            "initialize_during_edit_burst": [],
            "completion_during_edit_burst": [],
            "document_symbols": [],
            "completion": [],
            "definition": [],
            "hover": [],
        }
        diagnostic_counts = []
        symbol_count = 0
        try:
            _, elapsed = server.request("initialize", {
                "processId": None,
                "rootUri": workspace.as_uri(),
                "capabilities": {},
            })
            samples["initialize"].append(elapsed)
            server.notification("initialized", {})

            started = time.perf_counter_ns()
            server.notification("textDocument/didOpen", {
                "textDocument": {"uri": uri, "languageId": "rust", "version": 1, "text": source}
            })
            diagnostic = server.receive()
            samples["open_to_diagnostics"].append((time.perf_counter_ns() - started) / 1_000_000)
            assert diagnostic.get("method") == "textDocument/publishDiagnostics"
            diagnostic_counts.append(len(diagnostic.get("params", {}).get("diagnostics", [])))

            for index in range(1, args.rounds + 1):
                previous = f"EDIT{index - 1:06}"
                replacement = f"EDIT{index:06}"
                edit_line = next(
                    line_number for line_number, line in enumerate(source.splitlines())
                    if previous in line
                )
                edit_character = source.splitlines()[edit_line].index(previous)
                started = time.perf_counter_ns()
                server.notification("textDocument/didChange", {
                    "textDocument": {"uri": uri, "version": index + 1},
                    "contentChanges": [{
                        "range": {
                            "start": {"line": edit_line, "character": edit_character},
                            "end": {"line": edit_line, "character": edit_character + len(previous)},
                        },
                        "text": replacement,
                    }],
                })
                diagnostic = server.receive()
                diagnostic_counts.append(len(diagnostic.get("params", {}).get("diagnostics", [])))
                samples["incremental_edit_to_diagnostics"].append(
                    (time.perf_counter_ns() - started) / 1_000_000
                )
                source = source.replace(previous, replacement, 1)

            completion_results_incomplete = []
            completion_response = None
            for burst_round in range(args.burst_rounds):
                started = time.perf_counter_ns()
                for index in range(args.burst_size):
                    edit_number = args.rounds + burst_round * args.burst_size + index
                    previous = f"EDIT{edit_number:06}"
                    replacement = f"EDIT{edit_number + 1:06}"
                    lines = source.splitlines()
                    edit_line = next(
                        line_number for line_number, line in enumerate(lines)
                        if previous in line
                    )
                    edit_character = lines[edit_line].index(previous)
                    server.notification("textDocument/didChange", {
                        "textDocument": {"uri": uri, "version": edit_number + 2},
                        "contentChanges": [{
                            "range": {
                                "start": {"line": edit_line, "character": edit_character},
                                "end": {"line": edit_line, "character": edit_character + len(previous)},
                            },
                            "text": replacement,
                        }],
                    })
                    source = source.replace(previous, replacement, 1)
                _, elapsed = server.request("initialize", {
                    "processId": None,
                    "rootUri": workspace.as_uri(),
                    "capabilities": {},
                })
                samples["initialize_during_edit_burst"].append(elapsed)
                completion_id = server.next_id
                server.next_id += 1
                completion_started = time.perf_counter_ns()
                server.send({
                    "jsonrpc": "2.0",
                    "id": completion_id,
                    "method": "textDocument/completion",
                    "params": {
                        "textDocument": {"uri": uri},
                        "position": {"line": 2, "character": 20},
                    },
                })
                final_version = args.rounds + (burst_round + 1) * args.burst_size + 1
                final_diagnostics = None
                final_diagnostics_at = None
                completion_response = None
                while final_diagnostics is None or completion_response is None:
                    message = server.next_message()
                    if message.get("id") == completion_id:
                        completion_response = message
                        samples["completion_during_edit_burst"].append(
                            (time.perf_counter_ns() - completion_started) / 1_000_000
                        )
                        completion_results_incomplete.append(
                            bool(completion_response.get("result", {}).get("isIncomplete"))
                        )
                    if (message.get("method") == "textDocument/publishDiagnostics"
                            and message.get("params", {}).get("version") == final_version):
                        final_diagnostics = message
                        final_diagnostics_at = time.perf_counter_ns()
                diagnostic_counts.append(
                    len(final_diagnostics.get("params", {}).get("diagnostics", []))
                )
                samples["burst_edits_to_latest_diagnostics"].append(
                    (final_diagnostics_at - started) / 1_000_000
                )

            position = {"line": 2, "character": 20}
            completion_params = {"textDocument": {"uri": uri}, "position": position}
            symbol_line = next(
                line_number for line_number, line in enumerate(source.splitlines())
                if "function_0_0" in line
            )
            symbol_character = source.splitlines()[symbol_line].index("function_0_0") + 4
            symbol_position = {"line": symbol_line, "character": symbol_character}
            symbol_params = {"textDocument": {"uri": uri}, "position": symbol_position}
            for _ in range(args.rounds):
                response, elapsed = server.request(
                    "textDocument/documentSymbol", {"textDocument": {"uri": uri}}
                )
                samples["document_symbols"].append(elapsed)
                symbol_count = len(response.get("result", []))
                completion_response, elapsed = server.request(
                    "textDocument/completion", completion_params
                )
                if completion_response.get("result", {}).get("isIncomplete"):
                    raise RuntimeError("completion was marked incomplete after parsing finished")
                samples["completion"].append(elapsed)
                _, elapsed = server.request("textDocument/definition", symbol_params)
                samples["definition"].append(elapsed)
                _, elapsed = server.request("textDocument/hover", symbol_params)
                samples["hover"].append(elapsed)
            if any(diagnostic_counts):
                raise RuntimeError(f"benchmark corpus produced syntax diagnostics: {diagnostic_counts}")
        finally:
            server.shutdown()

    def summary(values: list[float]) -> dict:
        ordered = sorted(values)
        p95_index = max(0, math.ceil(0.95 * len(ordered)) - 1)
        return {
            "samples_ms": values,
            "p50_ms": statistics.median(values),
            "p95_ms": ordered[p95_index],
            "min_ms": ordered[0],
            "max_ms": ordered[-1],
        }

    cpu = platform.processor() or platform.machine()
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.lower().startswith("model name"):
                cpu = line.split(":", 1)[1].strip()
                break
    except OSError:
        pass

    return {
        "benchmark": "quill-lsp-syntax-latency",
        "quillc": str(args.quillc.resolve()),
        "source_bytes": len(source.encode()),
        "source_lines": source.count("\n") + 1,
        "modules": args.modules,
        "functions_per_module": args.functions_per_module,
        "rounds": args.rounds,
        "burst_size": args.burst_size,
        "burst_rounds": args.burst_rounds,
        "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
        "revision": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "cpu": cpu,
        "syntax_diagnostic_counts": diagnostic_counts,
        "document_symbol_count": symbol_count,
        "completion_during_edit_burst_incomplete": completion_results_incomplete,
        "measurements": {name: summary(values) for name, values in samples.items()},
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--quillc", type=Path, default=ROOT / "target/debug/quillc")
    parser.add_argument("--modules", type=int, default=40)
    parser.add_argument("--functions-per-module", type=int, default=20)
    parser.add_argument("--rounds", type=int, default=20)
    parser.add_argument("--burst-size", type=int, default=20)
    parser.add_argument("--burst-rounds", type=int, default=10)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if (args.rounds < 1 or args.modules < 1 or args.functions_per_module < 1
            or args.burst_size < 1 or args.burst_rounds < 1):
        parser.error("rounds, modules, functions-per-module, burst-size, and burst-rounds must be positive")
    result = measure(args)
    rendered = json.dumps(result, indent=2) + "\n"
    if args.output:
        args.output.write_text(rendered)
    print(rendered, end="")


if __name__ == "__main__":
    main()
