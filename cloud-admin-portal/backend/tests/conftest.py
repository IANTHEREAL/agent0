import os
import pytest
from fastapi.testclient import TestClient

os.environ.setdefault("PGTIKV_PD_ENDPOINTS", "127.0.0.1:2379")

from app.main import app
from app.database import get_db_manager
from app.models.db import Base
from app.config import get_settings


@pytest.fixture(scope="session", autouse=True)
def init_database():
    settings = get_settings()
    db_manager = get_db_manager(settings)
    Base.metadata.drop_all(bind=db_manager._engine)
    db_manager.create_tables()


@pytest.fixture
def client():
    return TestClient(app)
