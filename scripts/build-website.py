#!/usr/bin/env python3
"""Stage Quill's public website, excluding compiler builds and private files."""
import json
import shutil
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUTPUT = ROOT / "target/website"


def build():
    if OUTPUT.exists():
        shutil.rmtree(OUTPUT)
    OUTPUT.mkdir(parents=True)
    tracked = subprocess.check_output(["git", "ls-files", "-z"], cwd=ROOT).decode().split("\0")
    selected = []
    for name in filter(None, tracked):
        path = Path(name)
        if (
            (len(path.parts) == 1 and path.suffix == ".html")
            or name in ("README.md", "LICENSE")
            or path.parts[0] == "assets"
            or (path.parts[0] in ("docs", "benchmarks") and path.suffix in (".md", ".json"))
        ):
            selected.append(name)
    for name in selected:
        destination = OUTPUT / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(ROOT / name, destination)
    revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    (OUTPUT / "project-status.json").write_text(json.dumps({"revision": revision}) + "\n")
    shutil.copy2(ROOT / "website/_headers", OUTPUT / "_headers")
    for name in ("index.html", "compatibility.html", "project.html", "document.html", "assets/document.js"):
        if not (OUTPUT / name).is_file():
            raise RuntimeError(f"Missing website asset: {name}")
    print(f"Staged {len(selected)} public files at {OUTPUT} ({revision[:8]}).")


if __name__ == "__main__":
    build()
