#!/usr/bin/env python3
"""Serve Quill's website, rendering Markdown URLs through the document reader.

Usage: python3 serve.py --bind 0.0.0.0 --port 4173
Append ?raw=1 to a Markdown URL to retrieve its original source.
"""
import argparse
import io
import json
import subprocess
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

ROOT = Path(__file__).resolve().parent


class SiteHandler(SimpleHTTPRequestHandler):
    def end_headers(self):
        # This server is for local editing. Every response must be fresh,
        # including CSS, scripts, fonts, images, and error responses.
        self.send_header('Cache-Control', 'no-store')
        super().end_headers()

    def send_head(self):
        # An already-cached browser can still send validators after an edit.
        # Ignore them so SimpleHTTPRequestHandler returns the current body.
        for header in ('If-Modified-Since', 'If-None-Match'):
            if header in self.headers:
                del self.headers[header]
        url = urlsplit(self.path)
        if url.path == '/project-status.json':
            try:
                revision = subprocess.check_output(
                    ['git', 'rev-parse', '--verify', 'HEAD'], cwd=ROOT,
                    stderr=subprocess.DEVNULL, text=True, timeout=2,
                ).strip()
            except (subprocess.SubprocessError, OSError):
                revision = None
            data = json.dumps({'revision': revision}).encode('utf-8')
            self.send_response(200)
            self.send_header('Content-Type', 'application/json; charset=utf-8')
            self.send_header('Content-Length', str(len(data)))
            self.end_headers()
            return io.BytesIO(data)
        path = Path(self.translate_path(url.path))
        if path.suffix.lower() == '.md' and path.is_file():
            if parse_qs(url.query).get('raw') == ['1']:
                data = path.read_bytes()
                content_type = 'text/plain; charset=utf-8'
            else:
                data = (ROOT / 'document.html').read_bytes()
                content_type = 'text/html; charset=utf-8'
            self.send_response(200)
            self.send_header('Content-Type', content_type)
            self.send_header('Content-Length', str(len(data)))
            self.end_headers()
            return io.BytesIO(data)
        return super().send_head()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bind', default='0.0.0.0')
    parser.add_argument('--port', type=int, default=4173)
    args = parser.parse_args()
    server = ThreadingHTTPServer((args.bind, args.port), partial(SiteHandler, directory=str(ROOT)))
    print(f'Quill preview: http://{args.bind}:{args.port}', flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
