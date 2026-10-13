#!/usr/bin/env python3
"""Generate a helper-heavy workload where every function is codegen-reachable."""

from __future__ import annotations

import argparse
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--helpers", type=int, default=800)
    args = parser.parse_args()
    if args.helpers < 1:
        parser.error("--helpers must be positive")

    with args.output.open("w") as source:
        source.write("fn main() { std::process::exit(reachable_0(0)); }\n")
        for index in range(args.helpers):
            source.write(
                f"""fn reachable_{index}(input: i32) -> i32 {{
    let mut cursor = 0;
    let mut accumulator = input;
    while cursor < 4 {{
        accumulator = accumulator + cursor * 3;
        cursor = cursor + 1;
    }}
"""
            )
            if index + 1 < args.helpers:
                source.write(f"    reachable_{index + 1}(accumulator)\n")
            else:
                source.write("    42\n")
            source.write("}\n")
    print(f"generated {args.helpers} reachable helpers at {args.output}")


if __name__ == "__main__":
    main()
