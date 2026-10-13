#!/usr/bin/env python3
"""Run rust-analyzer parser fixtures through Quill's actual LSP process."""

from __future__ import annotations

import argparse
import glob
import json
import os
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
PARSER_VERSION = "0.0.358"
MAX_MESSAGE_BYTES = 64 * 1024 * 1024


def locate_fixtures() -> Path:
    cargo_home = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
    matches = sorted(
        Path(path) / "test_data" / "parser"
        for path in glob.glob(
            str(cargo_home / "registry" / "src" / "*" / f"ra_ap_parser-{PARSER_VERSION}")
        )
        if (Path(path) / "test_data" / "parser").is_dir()
    )
    if not matches:
        raise FileNotFoundError(
            f"ra_ap_parser {PARSER_VERSION} fixtures not found under {cargo_home}; "
            "pass --fixtures with the package's test_data/parser directory"
        )
    return matches[0]


class LspProcess:
    def __init__(self, executable: Path):
        self.process = subprocess.Popen(
            [str(executable), "lsp"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.next_id = 1

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
                stderr = self.process.stderr.read().decode(errors="replace")
                raise RuntimeError(f"language server exited before replying: {stderr}")
            if header in (b"\r\n", b"\n"):
                break
            name, _, value = header.partition(b":")
            if name.lower() == b"content-length":
                length = int(value.strip())
        if length is None or length > MAX_MESSAGE_BYTES:
            raise RuntimeError(f"invalid Content-Length header: {length}")
        payload = self.process.stdout.read(length)
        return json.loads(payload)

    def request(self, method: str, params: dict) -> dict:
        request_id = self.next_id
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        while True:
            response = self.receive()
            if response.get("id") == request_id:
                return response

    def wait_for_diagnostics(self, uri: str, version: int) -> list[dict]:
        while True:
            message = self.receive()
            if message.get("method") != "textDocument/publishDiagnostics":
                continue
            params = message.get("params", {})
            if params.get("uri") == uri and params.get("version") == version:
                return params.get("diagnostics", [])

    def shutdown(self) -> None:
        try:
            self.request("shutdown", {})
            self.send({"jsonrpc": "2.0", "method": "exit"})
            self.process.wait(timeout=10)
        finally:
            if self.process.poll() is None:
                self.process.kill()
                self.process.wait()


def make_workspace(root: Path, name: str, edition: str | None) -> Path:
    project = root / name
    project.mkdir(parents=True, exist_ok=True)
    manifest = [
        "[package]",
        f'name = "syntax-{name.replace("_", "-")}"',
        'version = "0.0.0"',
    ]
    if edition is not None:
        manifest.append(f'edition = "{edition}"')
    (project / "Cargo.toml").write_text("\n".join(manifest) + "\n")
    return project


def edition_directive(source: str) -> str:
    first_line = source.splitlines()[0] if source.splitlines() else ""
    prefixes = ("//@ edition: ", "// ")
    for prefix in prefixes:
        if first_line.startswith(prefix):
            year = first_line[len(prefix) :].strip()[:4]
            if year in {"2015", "2018", "2021", "2024"}:
                return year
    return "2024"


def collect_fixtures(root: Path) -> list[dict]:
    fixtures = []
    for path in sorted(root.rglob("*.rs")):
        relative = path.relative_to(root)
        if relative.parts[0] not in {"ok", "err", "inline"}:
            continue
        category = relative.parts[1] if relative.parts[0] == "inline" else relative.parts[0]
        if category not in {"ok", "err"}:
            continue
        source = path.read_text()
        fixtures.append(
            {
                "name": relative.as_posix(),
                "source": source,
                "expected_errors": category == "err",
                "edition": edition_directive(source),
            }
        )
    return fixtures


def edition_cases() -> list[dict]:
    return [
        {
            "name": "edition-2015-contextual-identifiers",
            "edition": "2015",
            "source": "fn async() {}\nfn await() {}\nfn dyn() {}\nfn try() {}\nfn gen() {}\n",
            "expected_errors": False,
        },
        {
            "name": "edition-2018-async-await",
            "edition": "2018",
            "source": "async fn run() { async {}.await; }\n",
            "expected_errors": False,
        },
        {
            "name": "edition-2021-dyn-trait-object",
            "edition": "2021",
            "source": "trait Item {}\nfn take(_: &dyn Item) {}\n",
            "expected_errors": False,
        },
        {
            "name": "edition-2024-raw-gen-identifier",
            "edition": "2024",
            "source": "fn r#gen() {}\n",
            "expected_errors": False,
        },
        {
            "name": "edition-2018-reserves-async",
            "edition": "2018",
            "source": "fn async() {}\n",
            "expected_errors": True,
        },
        {
            "name": "edition-2024-reserves-gen",
            "edition": "2024",
            "source": "fn gen() {}\n",
            "expected_errors": True,
        },
        {
            "name": "edition-inherited-from-workspace",
            "edition": "2021",
            "workspace_edition": "2021",
            "source": "async fn run() { async {}.await; }\n",
            "expected_errors": False,
        },
        {
            "name": "cargo-missing-edition-defaults-to-2015",
            "edition": None,
            "source": "fn async() {}\n",
            "expected_errors": False,
        },
        {
            "name": "unsupported-edition-warns-and-falls-back",
            "edition": "2027",
            "source": "fn plain() {}\n",
            "expected_errors": False,
            "expected_warning": "not supported by this parser",
        },
    ]


def recovery_cases() -> list[dict]:
    cases = [
        {
            "name": "recovery-unclosed-function",
            "source": "fn main( {\n",
            "repaired_source": "fn main() {}\n",
        },
        {
            "name": "recovery-unclosed-macro",
            "source": "macro_rules! m { ($x:expr) => { $x }\n",
            "repaired_source": "macro_rules! m { ($x:expr) => { $x }; }\n",
        },
        {
            "name": "recovery-missing-type",
            "source": "const VALUE: = 1;\n",
            "repaired_source": "const VALUE: u8 = 1;\n",
        },
        {
            "name": "recovery-unclosed-string",
            "source": 'fn main() { let value = "unfinished; }\n',
            "repaired_source": 'fn main() { let value = "finished"; }\n',
        },
        {
            "name": "recovery-unclosed-delimiters",
            "source": "fn main() { let value = [1, (2, 3]; }\n",
            "repaired_source": "fn main() { let value = [1, (2, 3)]; }\n",
        },
        {
            "name": "recovery-deep-unclosed-expression",
            "source": "fn deep() { let _ = " + "(" * 192 + "0" + ")" * 191 + "; }\n",
            "repaired_source": "fn deep() { let _ = " + "(" * 192 + "0" + ")" * 192 + "; }\n",
        },
        {
            "name": "recovery-long-unclosed-token-tree",
            "source": "macro_rules! deep { ($($t:tt)*) => { $($t)* } }\nfn main() { deep!({" + "[" * 160 + "0" + "]" * 159 + "}) }\n",
            "repaired_source": "macro_rules! deep { ($($t:tt)*) => { $($t)* } }\nfn main() { deep!({" + "[" * 160 + "0" + "]" * 160 + "}) }\n",
        },
    ]
    for case in cases:
        case.update(edition="2024", expected_errors=True)
    return cases


def run_crate_contract_tests(fixtures_root: Path) -> tuple[int, int, set[str]]:
    environment = os.environ.copy()
    environment["QUILL_RUST_PARSER_FIXTURES"] = str(fixtures_root.resolve())
    completed = subprocess.run(
        [
            os.environ.get("CARGO", "cargo"),
            "test",
            "-p",
            "quill-lsp",
            "--lib",
            "syntax_contract_tests",
            "--",
            "--nocapture",
        ],
        cwd=ROOT,
        env=environment,
        capture_output=True,
        text=True,
    )
    output = completed.stdout + completed.stderr
    if completed.returncode != 0:
        raise RuntimeError(f"quill-lsp syntax contract tests failed:\n{output[-6000:]}")
    match = re.search(r"verified upstream parser fixtures: (\d+)", output)
    if not match:
        raise RuntimeError(
            "quill-lsp syntax tests did not report upstream fixtures; "
            f"test output was:\n{output[-3000:]}"
        )
    kinds = re.search(r"observed syntax node kinds: ([A-Z_,]+)", output)
    if not kinds:
        raise RuntimeError("crate-level syntax test did not report observed syntax node kinds")
    edits = re.search(r"verified varied incremental edits: (\d+)", output)
    if not edits:
        raise RuntimeError("crate-level syntax test did not report varied incremental edits")
    return int(match.group(1)), int(edits.group(1)), set(kinds.group(1).split(","))


def grammar_production_coverage(fixtures_root: Path, observed_kinds: set[str]) -> dict:
    syntax_crate = fixtures_root.parents[2] / f"ra_ap_syntax-{PARSER_VERSION}"
    grammar_path = syntax_crate / "rust.ungram"
    if not grammar_path.is_file():
        raise FileNotFoundError(f"pinned parser grammar not found: {grammar_path}")
    productions = [
        match.group(1)
        for line in grammar_path.read_text().splitlines()
        if (match := re.match(r"^([A-Z][A-Za-z0-9]*)\s*=", line))
    ]
    generated_nodes = (syntax_crate / "src/ast/generated/nodes.rs").read_text()
    concrete_nodes = set(re.findall(r"^pub struct (\w+)", generated_nodes, re.MULTILINE))
    fragment_only_productions = {"MacroItems", "MacroStmts"}
    source_node_productions = [
        production
        for production in productions
        if production in concrete_nodes and production not in fragment_only_productions
    ]
    expected_kinds = {
        production: re.sub(r"(?<!^)([A-Z])", r"_\1", production).upper()
        for production in source_node_productions
    }
    missing = [
        production
        for production, kind in expected_kinds.items()
        if kind not in observed_kinds
    ]
    return {
        "grammar_production_count": len(productions),
        "concrete_source_node_production_count": len(source_node_productions),
        "concrete_source_node_productions_observed": len(source_node_productions)
        - len(missing),
        "fragment_only_productions": sorted(fragment_only_productions),
        "unobserved_grammar_productions": missing,
    }


def run(args: argparse.Namespace) -> dict:
    fixtures_root = args.fixtures or locate_fixtures()
    fixtures = collect_fixtures(fixtures_root)
    if not fixtures:
        raise RuntimeError(f"no parser .rs fixtures found under {fixtures_root}")

    cases = fixtures + edition_cases() + recovery_cases()
    (
        crate_contract_fixture_count,
        crate_contract_edit_count,
        observed_kinds,
    ) = run_crate_contract_tests(fixtures_root)
    if crate_contract_fixture_count != len(fixtures):
        raise RuntimeError(
            "crate and LSP fixture inventories differ: "
            f"{crate_contract_fixture_count} != {len(fixtures)}"
        )
    grammar_coverage = grammar_production_coverage(fixtures_root, observed_kinds)
    with tempfile.TemporaryDirectory(prefix="quill-rust-syntax-") as directory:
        workspace_root = Path(directory)
        requests = []
        expected_by_uri = {}
        source_by_uri = {}
        workspace_cache = {}
        for index, case in enumerate(cases):
            edition = case["edition"]
            workspace_key = edition if edition is not None else "default-2015"
            if "workspace_edition" in case:
                workspace_key = f"inherited-{case['workspace_edition']}"
            if workspace_key not in workspace_cache:
                if "workspace_edition" in case:
                    workspace = workspace_root / workspace_key
                    member = workspace / "member"
                    member.mkdir(parents=True)
                    (workspace / "Cargo.toml").write_text(
                        '[workspace]\nmembers = ["member"]\n\n'
                        f'[workspace.package]\nedition = "{case["workspace_edition"]}"\n'
                    )
                    (member / "Cargo.toml").write_text(
                        '[package]\nname = "edition-inherited"\nversion = "0.0.0"\n'
                        'edition.workspace = true\n'
                    )
                    workspace_cache[workspace_key] = member
                else:
                    workspace_cache[workspace_key] = make_workspace(
                        workspace_root, workspace_key.replace("-", "_"), edition
                    )
            project = workspace_cache[workspace_key]
            file_path = project / "src" / f"case-{index:04}.rs"
            file_path.parent.mkdir(parents=True, exist_ok=True)
            file_path.write_text(case["source"])
            uri = file_path.as_uri()
            expected_by_uri[uri] = case
            source_by_uri[uri] = case["source"]
            requests.append(
                {
                    "jsonrpc": "2.0",
                    "method": "textDocument/didOpen",
                    "params": {
                        "textDocument": {
                            "uri": uri,
                            "languageId": "rust",
                            "version": 1,
                            "text": case["source"],
                        }
                    },
                }
            )

        server = LspProcess(args.quillc.resolve())
        actual_by_uri = {}
        started = time.perf_counter()
        try:
            initialized = server.request(
                "initialize", {"processId": None, "rootUri": workspace_root.as_uri(), "capabilities": {}}
            )
            if "error" in initialized:
                raise RuntimeError(f"initialize failed: {initialized['error']}")
            server.send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
            for request in requests:
                server.send(request)

            while len(actual_by_uri) < len(expected_by_uri):
                message = server.receive()
                if message.get("method") != "textDocument/publishDiagnostics":
                    continue
                params = message.get("params", {})
                uri = params.get("uri")
                if uri in expected_by_uri and params.get("version") == 1:
                    actual_by_uri[uri] = params.get("diagnostics", [])

            incremental_mismatches = []
            recovery_edit_mismatches = []
            for uri, case in expected_by_uri.items():
                source = source_by_uri[uri]
                last_line = source.rsplit("\n", 1)[-1]
                line = source.count("\n")
                character = len(last_line.encode("utf-16-le")) // 2
                changed_source = source + " "
                server.send(
                    {
                        "jsonrpc": "2.0",
                        "method": "textDocument/didChange",
                        "params": {
                            "textDocument": {"uri": uri, "version": 2},
                            "contentChanges": [
                                {
                                    "range": {
                                        "start": {"line": line, "character": character},
                                        "end": {"line": line, "character": character},
                                    },
                                    "text": " ",
                                }
                            ],
                        },
                    }
                )
                incremental = server.wait_for_diagnostics(uri, 2)
                server.send(
                    {
                        "jsonrpc": "2.0",
                        "method": "textDocument/didClose",
                        "params": {"textDocument": {"uri": uri}},
                    }
                )
                server.send(
                    {
                        "jsonrpc": "2.0",
                        "method": "textDocument/didOpen",
                        "params": {
                            "textDocument": {
                                "uri": uri,
                                "languageId": "rust",
                                "version": 1,
                                "text": changed_source,
                            }
                        },
                    }
                )
                fresh = server.wait_for_diagnostics(uri, 1)
                if incremental != fresh:
                    incremental_mismatches.append(
                        {
                            "case": case["name"],
                            "incremental": incremental[:3],
                            "fresh": fresh[:3],
                        }
                    )

                repaired_source = case.get("repaired_source")
                if repaired_source is not None:
                    for version, edited_source, expected_errors in [
                        (3, repaired_source, False),
                        (4, changed_source, True),
                    ]:
                        server.send(
                            {
                                "jsonrpc": "2.0",
                                "method": "textDocument/didChange",
                                "params": {
                                    "textDocument": {"uri": uri, "version": version},
                                    "contentChanges": [{"text": edited_source}],
                                },
                            }
                        )
                        edit_diagnostics = server.wait_for_diagnostics(uri, version)
                        actual_errors = any(
                            diagnostic.get("severity") == 1
                            for diagnostic in edit_diagnostics
                        )
                        if actual_errors != expected_errors:
                            recovery_edit_mismatches.append(
                                {
                                    "case": case["name"],
                                    "version": version,
                                    "expected_errors": expected_errors,
                                    "actual_errors": actual_errors,
                                    "diagnostics": edit_diagnostics[:3],
                                }
                            )

                    server.send(
                        {
                            "jsonrpc": "2.0",
                            "method": "textDocument/didClose",
                            "params": {"textDocument": {"uri": uri}},
                        }
                    )
                    server.send(
                        {
                            "jsonrpc": "2.0",
                            "method": "textDocument/didOpen",
                            "params": {
                                "textDocument": {
                                    "uri": uri,
                                    "languageId": "rust",
                                    "version": 1,
                                    "text": changed_source,
                                }
                            },
                        }
                    )
                    broken_fresh = server.wait_for_diagnostics(uri, 1)
                    if edit_diagnostics != broken_fresh:
                        recovery_edit_mismatches.append(
                            {
                                "case": case["name"],
                                "reason": "broken incremental edit differs from fresh parse",
                                "incremental": edit_diagnostics[:3],
                                "fresh": broken_fresh[:3],
                            }
                        )
        finally:
            server.shutdown()

    mismatches = []
    for uri, case in expected_by_uri.items():
        diagnostics = actual_by_uri.get(uri)
        if diagnostics is None:
            mismatches.append({"case": case["name"], "reason": "no diagnostics notification"})
            continue
        actual_errors = any(diagnostic.get("severity") == 1 for diagnostic in diagnostics)
        expected_warning = case.get("expected_warning")
        if expected_warning and not any(
            expected_warning in diagnostic.get("message", "")
            for diagnostic in diagnostics
        ):
            mismatches.append(
                {
                    "case": case["name"],
                    "reason": f"expected warning containing {expected_warning!r}",
                    "diagnostics": diagnostics[:3],
                }
            )
        if actual_errors != case["expected_errors"]:
            mismatches.append(
                {
                    "case": case["name"],
                    "edition": case["edition"],
                    "expected_errors": case["expected_errors"],
                    "actual_errors": actual_errors,
                    "diagnostics": diagnostics[:3],
                }
            )

    return {
        "benchmark": "quill-lsp-rust-syntax-compatibility",
        "lsp_executable": str(args.quillc.resolve()),
        "parser_version": PARSER_VERSION,
        "fixture_root": str(fixtures_root),
        "fixture_count": len(fixtures),
        "accepted_fixtures": sum(not fixture["expected_errors"] for fixture in fixtures),
        "error_fixtures": sum(fixture["expected_errors"] for fixture in fixtures),
        "edition_probe_count": len(edition_cases()),
        "recovery_probe_count": len(recovery_cases()),
        "crate_contract_fixture_count": crate_contract_fixture_count,
        "crate_contract_varied_edits_checked": crate_contract_edit_count,
        "crate_contract_tests_passed": True,
        **grammar_coverage,
        "cases_checked": len(expected_by_uri),
        "incremental_roundtrips_checked": len(expected_by_uri),
        "incremental_mismatch_count": len(incremental_mismatches),
        "incremental_mismatches": incremental_mismatches,
        "recovery_edit_sequences_checked": len(recovery_cases()),
        "recovery_edit_mismatch_count": len(recovery_edit_mismatches),
        "recovery_edit_mismatches": recovery_edit_mismatches,
        "elapsed_seconds": round(time.perf_counter() - started, 3),
        "passed": (
            len(mismatches) == 0
            and len(incremental_mismatches) == 0
            and len(recovery_edit_mismatches) == 0
            and grammar_coverage["concrete_source_node_production_count"]
            == grammar_coverage["concrete_source_node_productions_observed"]
        ),
        "mismatch_count": len(mismatches),
        "mismatches": mismatches,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--quillc", type=Path, default=ROOT / "target/release/quillc")
    parser.add_argument("--fixtures", type=Path, help="ra_ap_parser test_data/parser directory")
    parser.add_argument("--output", type=Path, help="write the JSON result to this path")
    args = parser.parse_args()
    if not args.quillc.is_file():
        parser.error(f"quillc executable not found: {args.quillc}")

    try:
        result = run(args)
    except (FileNotFoundError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"syntax compatibility run failed: {error}", file=sys.stderr)
        return 2

    payload = json.dumps(result, indent=2) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(payload)
    print(payload, end="")
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
