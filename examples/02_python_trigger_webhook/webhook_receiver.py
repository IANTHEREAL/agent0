#!/usr/bin/env python3
"""
Standalone webhook receiver for testing cloud database triggers.

This script starts a simple HTTP server that receives webhook notifications
from the TiPG database triggers. It runs independently so you can expose it
via ngrok or other tunneling services.

Usage:
    python webhook_receiver.py [host] [port]

Examples:
    python webhook_receiver.py                    # Listen on 0.0.0.0:8765
    python webhook_receiver.py 127.0.0.1 8765     # Listen on 127.0.0.1:8765
    python webhook_receiver.py 0.0.0.0 9000       # Listen on 0.0.0.0:9000
"""

import json
import sys
from datetime import datetime
from http.server import HTTPServer, BaseHTTPRequestHandler


class WebhookHandler(BaseHTTPRequestHandler):
    """HTTP handler that logs all webhook POST requests."""

    def do_POST(self):
        # Read the request body
        content_length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(content_length).decode("utf-8")

        # Parse JSON payload
        try:
            payload = json.loads(body)
        except json.JSONDecodeError:
            payload = {"raw": body, "error": "Invalid JSON"}

        # Log the received webhook
        timestamp = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
        print(f"\n{'='*60}")
        print(f"[{timestamp}] Webhook received!")
        print(f"{'='*60}")
        print(f"Path: {self.path}")
        print(f"Payload: {json.dumps(payload, indent=2)}")
        print(f"{'='*60}\n")

        # Send success response
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"status":"ok","received":true}')

    def do_GET(self):
        """Handle GET requests for health checks."""
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.end_headers()
        self.wfile.write(b'Webhook receiver is running!')

    def log_message(self, format, *args):
        """Suppress default HTTP logging."""
        pass


def main():
    host = sys.argv[1] if len(sys.argv) > 1 else "0.0.0.0"
    port = int(sys.argv[2]) if len(sys.argv) > 2 else 8765

    print("=" * 60)
    print("TiPG Webhook Receiver")
    print("=" * 60)
    print(f"Listening on: http://{host}:{port}")
    print("Press Ctrl+C to stop")
    print("=" * 60)

    try:
        server = HTTPServer((host, port), WebhookHandler)
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n\nShutting down webhook receiver...")
        server.shutdown()
        print("Done.")


if __name__ == "__main__":
    main()
