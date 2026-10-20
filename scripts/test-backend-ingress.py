#!/usr/bin/env python3
"""Exercise additive nginx snippets in a private ephemeral localhost fixture.

Requires nginx. Never reads/installs existing nginx config or reloads its service.
"""
import http.server
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request

REPO = Path(__file__).resolve().parent.parent


class Upstream(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header('Content-Length', '0')
        self.end_headers()

    def log_message(self, *_):
        pass


with tempfile.TemporaryDirectory(prefix='remotevnc-nginx-') as directory:
    root = Path(directory)
    upstream = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Upstream)
    thread = threading.Thread(target=upstream.serve_forever, daemon=True)
    thread.start()
    with socket.socket() as reserve:
        reserve.bind(('127.0.0.1', 0))
        port = reserve.getsockname()[1]
    limits = (REPO / 'packaging/backend/nginx-http.conf').read_text()
    locations = (REPO / 'packaging/backend/nginx-locations.conf').read_text().replace(
        '127.0.0.1:18080', f'127.0.0.1:{upstream.server_port}').replace(
        '/var/log/nginx/remotevnc-access.log', str(root / 'access.log'))
    configuration = root / 'nginx.conf'
    configuration.write_text(f'''worker_processes 1;
pid {root}/nginx.pid;
error_log {root}/error.log crit;
events {{ worker_connections 128; }}
http {{ {limits}
server {{ listen 127.0.0.1:{port}; {locations} }}
}}
''')
    subprocess.run(['nginx', '-t', '-p', str(root), '-c', str(configuration)], check=True)
    nginx = subprocess.Popen(['nginx', '-p', str(root), '-c', str(configuration), '-g', 'daemon off;'])
    try:
        for _ in range(100):
            try:
                with socket.create_connection(('127.0.0.1', port), timeout=.1):
                    break
            except OSError:
                time.sleep(.02)
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

        def request(path):
            req = urllib.request.Request(f'http://127.0.0.1:{port}{path}', headers={'Authorization': 'Bearer fixture-authorization-secret'})
            try:
                with opener.open(req, timeout=3) as response:
                    return response.status
            except urllib.error.HTTPError as response:
                return response.code

        assert request('/login/callback?code=fixture-oauth-secret&state=fixture-state-secret') == 200
        started = time.monotonic()
        statuses = [request('/v1/enrollments') for _ in range(100)]
        elapsed = time.monotonic() - started
        assert 429 in statuses, 'public identity requests must be finite'
        assert statuses.count(200) <= 33 + int(2 * elapsed), statuses
        # Exercise an upstream failure too; nginx error logging must not leak original URL.
        upstream.shutdown()
        upstream.server_close()
        time.sleep(1)
        assert request('/login/callback?code=fixture-error-secret') == 502
    finally:
        nginx.terminate()
        nginx.wait(timeout=5)
        upstream.shutdown()
        upstream.server_close()
    logs = ''.join(p.read_text() for p in (root / 'access.log', root / 'error.log') if p.exists())
    for secret in ('fixture-oauth-secret', 'fixture-state-secret', 'fixture-authorization-secret', 'fixture-error-secret'):
        assert secret not in logs, 'proxy log leaked credential/query'
    assert '/login/callback' in logs
    print('nginx syntax, aggregate rate bound and callback/header/error-log privacy PASS')
