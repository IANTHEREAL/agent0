"""
Demo: TiPG Trigger + HTTP Extension Webhook Notifications

This demo shows how to use TiPG's AFTER triggers combined with the HTTP
extension to send webhook notifications whenever a table changes.

Flow:
  1. Start a local webhook receiver (simple HTTP server)
  2. Create products table, enable HTTP extension, set up triggers
  3. Perform INSERT / UPDATE / DELETE on products
  4. AFTER triggers fire asynchronously: each calls extensions.http_post()
     to POST a JSON payload to the webhook receiver, then logs the result
  5. Display received webhooks and the webhook_log table
"""

import json
import time
import threading
from http.server import HTTPServer, BaseHTTPRequestHandler

from sqlalchemy import text

from .database import Database
from .models import Product


# ============================================================================
# Webhook Receiver (runs in a background thread)
# ============================================================================

received_webhooks: list[dict] = []


class WebhookHandler(BaseHTTPRequestHandler):
    """Simple HTTP handler that accepts POST /webhook and records payloads."""

    def do_POST(self):
        content_length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(content_length).decode("utf-8")
        try:
            payload = json.loads(body)
        except json.JSONDecodeError:
            payload = {"raw": body}
        received_webhooks.append(payload)
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"ok":true}')

    def log_message(self, format, *args):
        # Suppress default logging to keep demo output clean
        pass


def start_webhook_server(host: str, port: int) -> HTTPServer:
    server = HTTPServer((host, port), WebhookHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server


# ============================================================================
# Demo
# ============================================================================

def demo(dsn: str, webhook_host: str = "127.0.0.1", webhook_port: int = 8765):
    webhook_url = f"http://{webhook_host}:{webhook_port}/webhook"

    # --- Step 1: Start webhook receiver ---
    print("=" * 60)
    print("TiPG Trigger + HTTP Extension Webhook Demo")
    print("=" * 60)
    print(f"\n[1] Starting webhook receiver on {webhook_url} ...")
    server = start_webhook_server(webhook_host, webhook_port)
    print("    Webhook receiver is running.")

    # --- Step 2: Set up database ---
    print(f"\n[2] Connecting to TiPG at {dsn} ...")
    db = Database(dsn)
    print("    Setting up tables, HTTP extension, and triggers ...")
    db.setup_all(webhook_url)
    print("    Done. Tables: products, webhook_log")
    print("    Triggers: trg_product_insert, trg_product_update, trg_product_delete")

    try:
        # --- Step 3: INSERT products ---
        print("\n[3] Inserting products ...")
        with db.session() as s:
            s.add(Product(name="Laptop", price=999.99, category="electronics"))
            s.add(Product(name="Headphones", price=49.99, category="electronics"))
            s.add(Product(name="Coffee Mug", price=12.50, category="kitchen"))
        print("    Inserted 3 products.")

        # AFTER triggers are async — wait for the trigger worker to process
        print("    Waiting for AFTER triggers to fire ...")
        time.sleep(1)

        # --- Step 4: UPDATE a product ---
        print("\n[4] Updating 'Laptop' price to 899.99 ...")
        with db.session() as s:
            laptop = s.query(Product).filter_by(name="Laptop").first()
            if laptop:
                laptop.price = 899.99
        time.sleep(1)

        # --- Step 5: DELETE a product ---
        print("\n[5] Deleting 'Coffee Mug' ...")
        with db.session() as s:
            mug = s.query(Product).filter_by(name="Coffee Mug").first()
            if mug:
                s.delete(mug)
        time.sleep(1)

        # --- Step 6: Show results ---
        print("\n" + "=" * 60)
        print("Results")
        print("=" * 60)

        # 6a: Current products
        print("\n-- Products table --")
        with db.session() as s:
            for p in s.query(Product).order_by(Product.id).all():
                print(f"   {p}")

        # 6b: Webhook log (database-side record of HTTP calls)
        print("\n-- Webhook log (from trigger) --")
        with db.session() as s:
            rows = s.execute(
                text("SELECT id, event_type, table_name, payload, http_status FROM webhook_log ORDER BY id")
            ).fetchall()
            if rows:
                for row in rows:
                    print(f"   id={row[0]}  event={row[1]}  table={row[2]}  payload={row[3]}  http_status={row[4]}")
            else:
                print("   (no entries yet — trigger worker may still be processing)")

        # 6c: Webhooks received by the HTTP server
        print("\n-- Webhooks received by HTTP server --")
        if received_webhooks:
            for i, wh in enumerate(received_webhooks, 1):
                print(f"   #{i}: {json.dumps(wh)}")
        else:
            print("   (none received yet)")

        print()

    finally:
        # --- Cleanup ---
        print("[*] Cleaning up ...")
        db.cleanup()
        server.shutdown()
        print("    Done.")
