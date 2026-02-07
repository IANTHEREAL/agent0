import os
from pathlib import Path
from typing import Generator, Optional

from sqlalchemy import create_engine
from sqlalchemy.orm import sessionmaker, Session

from .config import Settings, get_settings
from .models.db import Base


class DatabaseManager:
    def __init__(self, settings: Settings):
        self.settings = settings
        self._engine = None
        self._session_factory = None

    def _resolve_url(self, url: str) -> str:
        if not url.startswith("sqlite:///"):
            return url

        db_path = url.replace("sqlite:///", "")
        if not os.path.isabs(db_path):
            backend_dir = Path(__file__).parent.parent
            db_path = backend_dir / db_path.replace("backend/", "")
            db_path.parent.mkdir(parents=True, exist_ok=True)
            url = f"sqlite:///{db_path}"
        return url

    def init_db(self):
        if self._engine is not None:
            return

        db_url = self._resolve_url(self.settings.database_url)
        is_sqlite = db_url.startswith("sqlite")

        self._engine = create_engine(
            db_url,
            connect_args={"check_same_thread": False} if is_sqlite else {},
            echo=self.settings.debug,
        )
        self._session_factory = sessionmaker(
            autocommit=False,
            autoflush=False,
            bind=self._engine,
        )

    def create_tables(self):
        if self._engine is None:
            raise RuntimeError("Call init_db() first")
        Base.metadata.create_all(bind=self._engine)

    def get_session(self) -> Session:
        if self._session_factory is None:
            raise RuntimeError("Call init_db() first")
        return self._session_factory()

    def close(self):
        if self._engine:
            self._engine.dispose()


_db_manager: Optional[DatabaseManager] = None


def get_db_manager(settings: Optional[Settings] = None) -> DatabaseManager:
    global _db_manager
    if _db_manager is None:
        if settings is None:
            settings = get_settings()
        _db_manager = DatabaseManager(settings)
        _db_manager.init_db()
    return _db_manager


def get_db() -> Generator[Session, None, None]:
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
