"""Local HTTP front end for a trusted, phase-bound Gemini dispatcher.

The launcher owns the ledger and credentials. This server alone does not
isolate a candidate or prevent alternate network access.
"""
import hmac
from urllib.parse import parse_qsl, urlsplit
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from gemini_dispatch import MODEL, send


def server(address, budget, phase, token, api_key, *, dispatch=None):
    if not token or not api_key:
        raise ValueError("gateway credentials are required")
    if phase not in ("proposal", "development", "holdout"):
        raise ValueError("invalid gateway phase")

    class Handler(BaseHTTPRequestHandler):
        def setup(self):
            super().setup()
            self.connection.settimeout(30)

        def log_message(self, *_args):
            # Request URLs and headers must never enter diagnostic logs.
            pass

        def reply(self, status, body, content_type="application/json"):
            self.send_response(status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(body)
            self.close_connection = True

        def do_POST(self):
            try:
                target = urlsplit(self.path)
                pairs = parse_qsl(target.query, keep_blank_values=True,
                                  strict_parsing=True, max_num_fields=3)
                query = dict(pairs)
                if (target.scheme or target.netloc or target.fragment
                        or len(query) != len(pairs) or set(query) - {"key", "alt"}):
                    raise ValueError("invalid target")
            except ValueError:
                self.reply(400, b'{"error":"invalid target"}')
                return
            credentials = self.headers.get_all("x-goog-api-key", [])
            if "key" in query:
                credentials = credentials + [query["key"]]
            if len(credentials) != 1 or not hmac.compare_digest(
                credentials[0].encode(), token.encode()
            ):
                self.reply(401, b'{"error":"unauthorized"}')
                return
            paths = {
                f"/v1beta/models/{MODEL}:generateContent": False,
                f"/v1beta/models/{MODEL}:streamGenerateContent": True,
            }
            if (target.path not in paths or
                    query.get("alt") != ("sse" if paths.get(target.path) else None)):
                self.reply(404, b'{"error":"unsupported endpoint"}')
                return
            lengths = self.headers.get_all("Content-Length", [])
            if (self.headers.get_all("Transfer-Encoding") or len(lengths) != 1
                    or not lengths[0].isascii() or not lengths[0].isdecimal()
                    or not 0 < int(lengths[0]) <= 4_000_000):
                self.reply(400, b'{"error":"invalid body framing"}')
                return
            try:
                body = self.rfile.read(int(lengths[0]))
                if len(body) != int(lengths[0]):
                    raise ValueError("truncated request")
                status, content_type, result = (send if dispatch is None else dispatch)(
                    budget, phase, body, api_key, stream=paths[target.path]
                )
            except Exception:
                # Neither upstream errors nor ledger paths/credentials are exposed.
                self.reply(502, b'{"error":"dispatch refused or failed"}')
                return
            # Do not forward upstream headers, redirects or arbitrary MIME values.
            mime = "text/event-stream" if paths[target.path] else "application/json"
            self.reply(status, result, mime)

    return ThreadingHTTPServer(address, Handler)
