#!/usr/bin/env python3
"""Serve real-template reading fixtures on loopback. No account or mutation simulation."""
import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import mimetypes
import hashlib
import re
from urllib.parse import urlsplit

parser = argparse.ArgumentParser()
parser.add_argument('fixtures', type=Path)
parser.add_argument('--theme', choices=['dark', 'light'], default='dark')
parser.add_argument('--port', type=int, default=3102)
args = parser.parse_args()
source = Path(__file__).resolve().parents[2]

class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        path = urlsplit(self.path).path
        if path.startswith('/review/') and path.endswith('.html'):
            file = args.fixtures / args.theme / Path(path).name
        elif path == '/' or path.startswith('/f/'):
            file = args.fixtures / args.theme / 'feed.html'
        elif '/comments/' in path:
            file = args.fixtures / args.theme / 'discussion.html'
        elif path == '/register-sw.js':
            self.send_response(200); self.send_header('Content-Type', 'text/javascript'); self.end_headers(); return
        else:
            aliases = {'/fonts/source-sans-3.woff2': 'fonts/SourceSans3VF-Upright.ttf.woff2', '/fonts/source-serif-4.woff2': 'fonts/SourceSerif4-Regular.ttf.woff2', '/touch-icon-iphone.png': 'apple-touch-icon.png'}
            file = source / 'static' / aliases.get(path, path.lstrip('/'))
            if not file.resolve().is_relative_to((source / 'static').resolve()):
                self.send_error(404); return
        if not file.is_file():
            self.send_error(404); return
        body = file.read_bytes()
        if file.suffix == '.html':
            html = body.decode()
            for asset in ['style.css', 'vale-interactions.js']:
                digest = hashlib.sha256((source / 'static' / asset).read_bytes()).hexdigest()[:12]
                html = re.sub(r'/' + re.escape(asset) + r'\?[^"\s]+', '/' + asset + '?review=' + digest, html)
            body = html.encode()
        self.send_response(200)
        self.send_header('Content-Type', mimetypes.guess_type(file)[0] or 'application/octet-stream')
        self.send_header('Cache-Control', 'no-store')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers(); self.wfile.write(body)
    def log_message(self, *_):
        pass

print(f'Reading fixtures: http://127.0.0.1:{args.port}/ ({args.theme})', flush=True)
ThreadingHTTPServer(('127.0.0.1', args.port), Handler).serve_forever()
