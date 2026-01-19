"""Database connection management and session handling."""

import os
from pathlib import Path
from typing import Generator

from sqlalchemy import text
from sqlalchemy import create_engine
from sqlalchemy.orm import sessionmaker, Session

from .config import Settings, get_settings
from .models.db import Base


class DatabaseManager:
    """Manages database connections and initialization."""

    def __init__(self, settings: Settings):
        """Initialize database manager with settings."""
        self.settings = settings
        self._engine = None
        self._session_factory = None

    def _resolve_database_path(self, url: str) -> str:
        """Resolve SQLite database path relative to project root."""
        if not url.startswith("sqlite:///"):
            return url

        # Extract path from URL
        db_path = url.replace("sqlite:///", "")

        # If path is relative, make it relative to project root
        if not os.path.isabs(db_path):
            # Get project root (backend directory)
            backend_dir = Path(__file__).parent.parent
            db_path = backend_dir / db_path.replace("backend/", "")

            # Ensure the directory exists
            db_path.parent.mkdir(parents=True, exist_ok=True)

            # Convert back to URL format
            url = f"sqlite:///{db_path}"

        return url

    def init_db(self):
        """Initialize database engine and session factory."""
        if self._engine is None:
            db_url = self._resolve_database_path(self.settings.database_url)

            # Create engine with proper SQLite settings
            self._engine = create_engine(
                db_url,
                connect_args={"check_same_thread": False} if db_url.startswith("sqlite") else {},
                echo=self.settings.debug,
            )

            # Create session factory
            self._session_factory = sessionmaker(
                autocommit=False,
                autoflush=False,
                bind=self._engine,
            )

    def create_tables(self):
        """Create all database tables."""
        if self._engine is None:
            raise RuntimeError("Database not initialized. Call init_db() first.")

        Base.metadata.create_all(bind=self._engine)
        self._migrate_schema()

    def _migrate_schema(self):
        """Apply lightweight, idempotent schema migrations (SQLite-first)."""
        if self._engine is None:
            return

        try:
            dialect = self._engine.dialect.name
        except Exception:
            return

        if dialect != "sqlite":
            return

        with self._engine.begin() as conn:
            rows = conn.execute(text("PRAGMA table_info(tenants)")).fetchall()
            existing_cols = set()
            for r in rows:
                try:
                    existing_cols.add(r._mapping["name"])
                except Exception:
                    existing_cols.add(r[1])

            if "observability_user" not in existing_cols:
                conn.exec_driver_sql(
                    "ALTER TABLE tenants ADD COLUMN observability_user VARCHAR(255)"
                )
            if "observability_password" not in existing_cols:
                conn.exec_driver_sql(
                    "ALTER TABLE tenants ADD COLUMN observability_password VARCHAR(255)"
                )
            if "keyspace" not in existing_cols:
                conn.exec_driver_sql(
                    "ALTER TABLE tenants ADD COLUMN keyspace VARCHAR(64)"
                )

            audit_rows = conn.execute(text("PRAGMA table_info(audit_logs)")).fetchall()
            audit_cols = set()
            for r in audit_rows:
                try:
                    audit_cols.add(r._mapping["name"])
                except Exception:
                    audit_cols.add(r[1])

            if "tenant_id" not in audit_cols:
                conn.exec_driver_sql(
                    "ALTER TABLE audit_logs ADD COLUMN tenant_id VARCHAR(14)"
                )

    def get_session(self) -> Session:
        """Create a new database session."""
        if self._session_factory is None:
            raise RuntimeError("Database not initialized. Call init_db() first.")

        return self._session_factory()

    def close(self):
        """Close database connections."""
        if self._engine:
            self._engine.dispose()


# Global database manager instance
_db_manager: DatabaseManager = None


def get_db_manager(settings: Settings = None) -> DatabaseManager:
    """Get or create global database manager instance."""
    global _db_manager

    if _db_manager is None:
        if settings is None:
            settings = get_settings()
        _db_manager = DatabaseManager(settings)
        _db_manager.init_db()

    return _db_manager


def get_db() -> Generator[Session, None, None]:
    """FastAPI dependency for database sessions.

    Usage:
        @app.get("/endpoint")
        def endpoint(db: Session = Depends(get_db)):
            # Use db session
            pass
    """
    db_manager = get_db_manager()
    session = db_manager.get_session()
    try:
        yield session
        session.commit()
    except Exception:
        session.rollback()
        raise
    finally:
        session.close()
