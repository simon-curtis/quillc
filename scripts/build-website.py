#!/usr/bin/env python3
"""Compatibility entry point for the Vite+ production build (run npm ci first)."""
import subprocess
from pathlib import Path

if __name__ == '__main__':
    subprocess.run(['npm', 'run', 'build'], cwd=Path(__file__).resolve().parent.parent, check=True)
