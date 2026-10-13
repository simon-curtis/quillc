#!/usr/bin/env python3
"""Generate a dead-helper-heavy standalone source for the codegen experiment."""

import argparse
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("output", type=Path)
    parser.add_argument("--helpers", type=int, default=3400)
    parser.add_argument("--public", action="store_true", help="make unused helpers public to test executable-root pruning")
    args = parser.parse_args()
    if args.helpers < 1:
        parser.error("--helpers must be positive")

    with args.output.open("w") as source:
        source.write("fn live(value: i32) -> i32 { value + 1 }\n")
        source.write("fn main() { std::process::exit(live(41)); }\n")
        for index in range(args.helpers):
            source.write(
                f"""{'pub ' if args.public else ''}fn dead_{index}(arg: i32) -> i32 {{
    let mut cursor = arg;
    let mut accum = 0;
    while cursor < arg + 4 {{
        accum = accum + cursor * 3;
        cursor = cursor + 1;
    }}
    accum
}}
"""
            )
    print(f"generated {args.helpers} dead helpers at {args.output}")


if __name__ == "__main__":
    main()
