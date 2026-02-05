"""
Entry point for the trigger webhook demo.

Usage:
    python run.py <postgresql_dsn> [webhook_host] [webhook_port]

Examples:
    python run.py postgresql://admin:admin@127.0.0.1:5433/postgres
    python run.py postgresql://admin:admin@127.0.0.1:5433/postgres 0.0.0.0 9000
"""

import sys
from trigger_webhook.main import demo

if __name__ == "__main__":
    if len(sys.argv) < 2:
        print("Usage: python run.py <postgresql_dsn> [webhook_host] [webhook_port]")
        print("Example: python run.py postgresql://admin:admin@127.0.0.1:5433/postgres")
        sys.exit(1)

    dsn = sys.argv[1]
    host = sys.argv[2] if len(sys.argv) > 2 else "127.0.0.1"
    port = int(sys.argv[3]) if len(sys.argv) > 3 else 8765

    demo(dsn, webhook_host=host, webhook_port=port)
