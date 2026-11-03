"""Exercise publication over HTTP without real credentials or forge writes."""
import hashlib
from http.server import BaseHTTPRequestHandler, HTTPServer
import importlib.util
import json
import os
from pathlib import Path
import tempfile
from threading import Thread
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('publish', Path(__file__).with_name('publish.py'))
publisher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(publisher)


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def respond(self):
        body = self.rfile.read(int(self.headers.get('Content-Length', 0)))
        self.server.calls.append((self.command, self.path, dict(self.headers), body))
        code, value = 200, {}
        if self.server.reject:
            code = 403
        elif self.command == 'GET':
            value = self.server.existing
            if value is None:
                code = 404
        self.send_response(code)
        self.send_header('Content-Type', 'application/json')
        self.end_headers()
        self.wfile.write(json.dumps(value).encode())

    do_GET = do_POST = do_PUT = respond


class PublicationTest(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        previous = Path.cwd()
        os.chdir(self.directory.name)
        self.addCleanup(os.chdir, previous)
        root = Path('release')
        root.mkdir()
        for name in ('Pneu-macOS.zip', 'pneu_2.2.2_amd64.deb', 'pneu-linux-x86_64.tar.gz'):
            (root / name).write_bytes(name.encode())
        checksum = hashlib.sha256(b'Pneu-macOS.zip').hexdigest()
        (root / 'macOS-SHA256SUMS.txt').write_text(f'{checksum}  Pneu-macOS.zip\n')
        self.server = HTTPServer(('127.0.0.1', 0), Handler)
        self.server.calls, self.server.existing, self.server.reject = [], None, False
        thread = Thread(target=self.server.serve_forever)
        thread.start()
        def close():
            self.server.shutdown()
            thread.join()
            self.server.server_close()
        self.addCleanup(close)
        self.env = patch.dict(os.environ, CI_API_V4_URL=f'http://127.0.0.1:{self.server.server_port}/api/v4',
                              CI_PROJECT_ID='42', CI_JOB_TOKEN='test-only-token', RELEASE_TAG='v2.2.2', CI_JOB_ID='123')
        self.env.start()
        self.addCleanup(self.env.stop)

    def test_create_uploads_checksums_and_stable_asset_links(self):
        publisher.publish()
        calls = self.server.calls
        self.assertEqual([x[0] for x in calls], ['PUT'] * 4 + ['GET', 'POST'])
        for method, path, headers, body in calls:
            self.assertEqual(headers['Job-Token'], 'test-only-token')
            self.assertTrue(path.startswith('/api/v4/projects/42/'))
        sums = calls[3][3].decode()
        for name in ('Pneu-macOS.zip', 'pneu_2.2.2_amd64.deb', 'pneu-linux-x86_64.tar.gz'):
            self.assertIn(f'{hashlib.sha256(name.encode()).hexdigest()}  {name}\n', sums)
        release = json.loads(calls[-1][3])
        self.assertEqual(release['tag_name'], 'v2.2.2')
        self.assertIn('https://stantonmatthewj.gitlab.io/pneu/', release['description'])
        links = release['assets']['links']
        self.assertEqual(len(links), 4)
        self.assertEqual(links[0]['direct_asset_path'], '/Pneu-macOS.zip')
        self.assertIn('/packages/generic/pneu/v2.2.2-123/', links[0]['url'])

    def test_republish_updates_existing_links_and_adds_missing_links(self):
        self.server.existing = {'assets': {'links': [{'name': 'Pneu-macOS.zip', 'id': 7}]}}
        publisher.publish()
        writes = [(method, path) for method, path, _, _ in self.server.calls[5:]]
        self.assertEqual(writes[0], ('PUT', '/api/v4/projects/42/releases/v2.2.2'))
        self.assertEqual(writes[1], ('PUT', '/api/v4/projects/42/releases/v2.2.2/assets/links/7'))
        self.assertEqual([method for method, _ in writes[2:]], ['POST'] * 3)

    def test_bad_checksum_writes_nothing(self):
        Path('release/Pneu-macOS.zip').write_bytes(b'corrupted')
        with self.assertRaisesRegex(ValueError, 'checksum mismatch'):
            publisher.publish()
        self.assertEqual(self.server.calls, [])

    def test_api_failure_stops_without_logging_credentials(self):
        self.server.reject = True
        with self.assertRaisesRegex(RuntimeError, 'HTTP 403') as caught:
            publisher.publish()
        self.assertNotIn('test-only-token', str(caught.exception))
        self.assertEqual(len(self.server.calls), 1)


if __name__ == '__main__':
    unittest.main()
