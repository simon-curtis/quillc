#!/usr/bin/env python3
"""Generate a deterministic catalog/ingestion workload for compiler builds."""

import shutil
from pathlib import Path

FIXTURE = Path(__file__).parent / "catalog-flow"
MODULES = 41
PACKAGES = [
    ("catalog-model", range(0, 5), None),
    ("catalog-parse", range(5, 10), "catalog-model"),
    ("catalog-ingest", range(10, 15), "catalog-parse"),
    ("catalog-validate", range(15, 20), "catalog-ingest"),
    ("catalog-index", range(20, 25), "catalog-validate"),
    ("catalog-report", range(25, 30), "catalog-index"),
    ("catalog-audit", range(30, 35), "catalog-model"),
    ("catalog-flow", range(35, 41), "catalog-report"),
]


def render_module(index: int) -> str:
    name = f"shard_{index:02}"
    lines = [
        "use std::collections::BTreeMap;",
        "",
        "#[derive(Clone, Debug, PartialEq, Eq)]",
        f"pub struct Record{index} {{",
        "    pub key: String,",
        "    pub revision: u64,",
        "    pub labels: Vec<String>,",
        "    pub fields: BTreeMap<String, String>,",
        "}",
        "",
        f"impl Record{index} {{",
        "    pub fn new(key: impl Into<String>, revision: u64) -> Self {",
        "        Self { key: key.into(), revision, labels: Vec::new(), fields: BTreeMap::new() }",
        "    }",
        "    pub fn label(mut self, value: impl Into<String>) -> Self {",
        "        self.labels.push(value.into());",
        "        self",
        "    }",
        "    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) {",
        "        self.fields.insert(key.into(), value.into());",
        "    }",
        "    pub fn stable_id(&self) -> u64 {",
        "        self.key.bytes().fold(self.revision, |state, byte| state.wrapping_mul(33).wrapping_add(u64::from(byte)))",
        "    }",
        "}",
        "",
        "pub fn ingest<T: AsRef<str>>(record: &mut Record" + str(index) + ", fields: &[(T, T)]) -> usize {",
        "    for (key, value) in fields { record.insert(key.as_ref(), value.as_ref()); }",
        "    record.fields.len()",
        "}",
        "",
        "pub fn summarize(records: &[Record" + str(index) + "]) -> (usize, u64) {",
        "    records.iter().fold((0, 0), |(count, hash), record| (count + 1, hash ^ record.stable_id()))",
        "}",
        "",
    ]
    # A realistic mixture of small validation/indexing operations creates
    # many independent functions while keeping a small shared runtime surface.
    for n in range(36):
        lines.extend([
            f"pub fn normalize_{n}(input: &str) -> String {{",
            "    let mut output = String::with_capacity(input.len());",
            "    let mut separator = false;",
            "    for ch in input.chars() {",
            "        if ch.is_ascii_alphanumeric() {",
            "            if separator && !output.is_empty() { output.push('-'); }",
            "            output.extend(ch.to_lowercase());",
            "            separator = false;",
            "        } else { separator = true; }",
            "    }",
            "    output",
            "}",
            f"pub fn validate_{n}(record: &Record{index}) -> Result<(), &'static str> {{",
            "    if record.key.is_empty() { return Err(\"empty key\"); }",
            "    if record.revision == 0 { return Err(\"revision must be positive\"); }",
            "    if record.labels.iter().any(|label| label.trim().is_empty()) { return Err(\"empty label\"); }",
            "    Ok(())",
            "}",
            "",
        ])
    # Private unreachable work probes the cost of conservative whole-crate
    # analysis without exporting those helpers as part of the library API.
    for n in range(6):
        lines.extend([
            f"fn unused_rebuild_index_{n}(records: &[Record{index}]) -> BTreeMap<u64, String> {{",
            "    let mut index = BTreeMap::new();",
            "    for record in records { index.insert(record.stable_id(), record.key.clone()); }",
            "    index",
            "}",
            "",
        ])
    return "\n".join(lines)


def main() -> None:
    if FIXTURE.exists():
        shutil.rmtree(FIXTURE)
    FIXTURE.mkdir(parents=True)
    member_paths = []
    for package_index, (package, module_indices, dependency) in enumerate(PACKAGES):
        package_dir = FIXTURE / package
        source_dir = package_dir / "src"
        source_dir.mkdir(parents=True)
        member_paths.append(package)
        manifest = [
            "[package]",
            f'name = "{package}"',
            'version = "0.1.0"',
            'edition = "2024"',
            "",
        ]
        if dependency:
            manifest.extend([
                "[dependencies]",
                f'{dependency.replace("-", "_")} = {{ package = "{dependency}", path = "../{dependency}" }}',
                "",
            ])
        (package_dir / "Cargo.toml").write_text("\n".join(manifest), encoding="utf-8")
        for index in module_indices:
            (source_dir / f"shard_{index:02}.rs").write_text(render_module(index), encoding="utf-8")
        module_list = list(module_indices)
        lib = "\n".join(f"pub mod shard_{i:02};" for i in module_list) + "\n"
        dependency_call = f"{dependency.replace('-', '_')}::package_digest() ^ " if dependency else "0 ^ "
        local_calls = " ^ ".join(
            f"shard_{i:02}::summarize(&[]).1 ^ shard_{i:02}::normalize_0(\"catalog record\").len() as u64"
            for i in module_list
        )
        lib += f"pub fn package_digest() -> u64 {{ {dependency_call}{local_calls} }}\n"
        if package == "catalog-flow":
            first = module_list[0]
            lib += (
                "pub fn run() -> (usize, u64) {\n"
                "    let mut records = Vec::new();\n"
                "    for revision in 1..=128 {\n"
                f'        let mut record = shard_{first:02}::Record{first}::new(format!("item-{{revision}}"), revision).label("ingested");\n'
                f'        shard_{first:02}::ingest(&mut record, &[("source", "fixture"), ("status", "ready")]);\n'
                f"        if shard_{first:02}::validate_0(&record).is_ok() {{ records.push(record); }}\n"
                "    }\n"
                f"    let (count, digest) = shard_{first:02}::summarize(&records);\n"
                "    (count, digest ^ package_digest())\n}\n"
            )
            (source_dir / "main.rs").write_text(
                "fn main() {\n    let (records, digest) = catalog_flow::run();\n"
                '    println!("catalog-flow records={records} digest={digest:016x}");\n}\n', encoding="utf-8"
            )
        (source_dir / "lib.rs").write_text(lib, encoding="utf-8")
    (FIXTURE / "Cargo.toml").write_text(
        "[workspace]\nresolver = \"3\"\nmembers = [\n"
        + "".join(f'    "{member}",\n' for member in member_paths)
        + "]\ndefault-members = [\n"
        + "".join(f'    "{member}",\n' for member in member_paths)
        + "]\n",
        encoding="utf-8",
    )
    count = sum(len(path.read_text(encoding="utf-8").splitlines()) for path in FIXTURE.rglob("*.rs"))
    print(f"generated {count} Rust lines across {len(PACKAGES)} Cargo packages in {FIXTURE}")


if __name__ == "__main__":
    main()
