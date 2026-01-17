#!/usr/bin/env python3
"""Database initialization script for cloud-admin-portal.

This script creates the database tables for the portal.
It can be run manually or will be automatically run on server startup.

Usage:
    python init_db.py
"""

import sys
from pathlib import Path

# Add parent directory to path for imports
sys.path.insert(0, str(Path(__file__).parent))

from app.database import get_db_manager
from app.config import get_settings


def main():
    """Initialize the database."""
    print("=" * 80)
    print("Cloud Admin Portal - Database Initialization")
    print("=" * 80)

    # Get settings
    settings = get_settings()
    print(f"\nDatabase URL: {settings.database_url}")

    # Initialize database manager
    print("\nInitializing database manager...")
    db_manager = get_db_manager(settings)

    # Create tables
    print("Creating database tables...")
    db_manager.create_tables()

    print("\n✓ Database initialized successfully!")
    print("\nTables created:")
    print("  - tenants (tenant metadata)")
    print("  - audit_logs (operation audit trail)")

    print("\nThe server will automatically sync existing TiKV keyspaces on startup.")
    print("=" * 80)


if __name__ == "__main__":
    main()
