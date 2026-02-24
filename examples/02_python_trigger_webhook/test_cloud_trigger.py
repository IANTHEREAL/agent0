#!/usr/bin/env python3
"""
Test script to verify cloud db9 database can trigger webhooks.

This script uses an online webhook testing service instead of a local server,
making it perfect for testing cloud database triggers.

Usage:
    python test_cloud_trigger.py <database_url> <webhook_url>

Examples:
    # Using webhook.site
    python test_cloud_trigger.py \
        postgresql://admin:admin@your-cloud-db:5433/postgres \
        https://webhook.site/your-unique-id

    # Using requestbin
    python test_cloud_trigger.py \
        postgresql://admin:admin@your-cloud-db:5433/postgres \
        https://requestbin.com/your-bin-id

Recommended online webhook services:
    - https://webhook.site (no registration required)
    - https://requestbin.com
    - https://pipedream.com
    - https://beeceptor.com
"""

import sys
import time
from sqlalchemy import text

from trigger_webhook.database import Database
from trigger_webhook.models import Product


def test_triggers(dsn: str, webhook_url: str):
    """Test cloud database triggers with an online webhook service."""

    print("=" * 70)
    print("db9 Cloud Database → Webhook Trigger Test")
    print("=" * 70)
    print(f"\nDatabase: {dsn}")
    print(f"Webhook:  {webhook_url}")
    print()

    # --- Step 1: Connect to database ---
    print("[1] Connecting to cloud database...")
    db = Database(dsn)

    # --- Step 2: Setup tables, extension, and triggers ---
    print("[2] Setting up tables, HTTP extension, and triggers...")
    print("    This will:")
    print("    - Drop existing tables and triggers (if any)")
    print("    - Create 'products' and 'webhook_log' tables")
    print("    - Enable HTTP extension")
    print("    - Create AFTER triggers for INSERT/UPDATE/DELETE")
    print()

    try:
        db.setup_all(webhook_url)
        print("    ✓ Setup completed!")
        print()

        # --- Step 3: Test INSERT trigger ---
        print("[3] Testing INSERT trigger...")
        print("    Inserting 3 products...")
        with db.session() as s:
            s.add(Product(name="Laptop", price=999.99, category="electronics"))
            s.add(Product(name="Headphones", price=49.99, category="electronics"))
            s.add(Product(name="Coffee Mug", price=12.50, category="kitchen"))
        print("    ✓ Products inserted")
        print("    ⏳ Waiting for AFTER triggers to fire (3 seconds)...")
        time.sleep(3)

        # --- Step 4: Test UPDATE trigger ---
        print("\n[4] Testing UPDATE trigger...")
        print("    Updating 'Laptop' price to 899.99...")
        with db.session() as s:
            laptop = s.query(Product).filter_by(name="Laptop").first()
            if laptop:
                laptop.price = 899.99
        print("    ✓ Product updated")
        print("    ⏳ Waiting for trigger to fire (2 seconds)...")
        time.sleep(2)

        # --- Step 5: Test DELETE trigger ---
        print("\n[5] Testing DELETE trigger...")
        print("    Deleting 'Coffee Mug'...")
        with db.session() as s:
            mug = s.query(Product).filter_by(name="Coffee Mug").first()
            if mug:
                s.delete(mug)
        print("    ✓ Product deleted")
        print("    ⏳ Waiting for trigger to fire (2 seconds)...")
        time.sleep(2)

        # --- Step 6: Show results ---
        print("\n" + "=" * 70)
        print("Results")
        print("=" * 70)

        # 6a: Current products in database
        print("\n📦 Products in database:")
        with db.session() as s:
            products = s.query(Product).order_by(Product.id).all()
            if products:
                for p in products:
                    print(f"   {p}")
            else:
                print("   (no products)")

        # 6b: Webhook log (database-side record)
        print("\n📝 Webhook log (stored in database):")
        with db.session() as s:
            rows = s.execute(
                text("""
                    SELECT id, event_type, table_name, http_status, payload
                    FROM webhook_log
                    ORDER BY id
                """)
            ).fetchall()
            if rows:
                for row in rows:
                    print(f"   [{row[0]}] {row[1]} on {row[2]} → HTTP {row[3]}")
                    print(f"       Payload: {row[4]}")
            else:
                print("   ⚠️  No webhook logs yet.")
                print("   This might mean:")
                print("   - Triggers haven't fired yet (try waiting longer)")
                print("   - DB9_TRIGGER_ENABLED is not set to 'true'")
                print("   - HTTP extension is not working properly")

        # 6c: Instructions for checking online webhook service
        print("\n🌐 Check your webhook service:")
        print(f"   Go to: {webhook_url}")
        print("   You should see 5 POST requests:")
        print("   - 3 INSERT events (Laptop, Headphones, Coffee Mug)")
        print("   - 1 UPDATE event (Laptop price change)")
        print("   - 1 DELETE event (Coffee Mug)")
        print()

        # 6d: Troubleshooting
        if not rows or len(rows) < 5:
            print("⚠️  Troubleshooting:")
            print("   If you don't see all 5 webhook logs:")
            print()
            print("   1. Check db9 server environment variables:")
            print("      ✓ DB9_TRIGGER_ENABLED=true")
            print("      ✓ DB9_HTTP_ALLOW_INSECURE=true (if using HTTP)")
            print()
            print("   2. Check db9 server logs for errors")
            print()
            print("   3. Verify the webhook URL is accessible:")
            print(f'      curl -X POST {webhook_url} -d \'{{"test":"ok"}}\'')
            print()
            print("   4. Wait longer (triggers are async, may take time)")
            print()

    except Exception as e:
        print(f"\n❌ Error: {e}")
        import traceback
        traceback.print_exc()
    finally:
        # --- Cleanup ---
        print("\n[6] Cleaning up...")
        print("    Dropping tables and triggers...")
        db.cleanup()
        print("    ✓ Cleanup completed")
        print()
        print("=" * 70)
        print("Test finished!")
        print("=" * 70)


def main():
    if len(sys.argv) < 3:
        print("Usage: python test_cloud_trigger.py <database_url> <webhook_url>")
        print()
        print("Examples:")
        print("  python test_cloud_trigger.py \\")
        print("    postgresql://admin:admin@your-cloud-db:5433/postgres \\")
        print("    https://webhook.site/your-unique-id")
        print()
        print("Recommended webhook services:")
        print("  - https://webhook.site (instant, no registration)")
        print("  - https://requestbin.com")
        print("  - https://pipedream.com")
        print()
        sys.exit(1)

    dsn = sys.argv[1]
    webhook_url = sys.argv[2]

    test_triggers(dsn, webhook_url)


if __name__ == "__main__":
    main()
