#!/usr/bin/env python3
"""Compatibility launcher for Vite+. Preferred command: npm run dev."""
import argparse
import os
from pathlib import Path

if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bind', default='0.0.0.0')
    parser.add_argument('--port', type=int, default=4173)
    args = parser.parse_args()
    os.chdir(Path(__file__).resolve().parent)
    os.execvp('npm', ['npm', 'run', 'dev', '--', '--host', args.bind, '--port', str(args.port)])
