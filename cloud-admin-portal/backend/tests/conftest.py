"""Pytest fixtures for API tests."""

import os
import pytest
from fastapi.testclient import TestClient

os.environ.setdefault("PGTIKV_PD_ENDPOINTS", "127.0.0.1:2379")

from app.main import app
from app.database import get_db_manager
from app.config import get_settings


@pytest.fixture(scope="session", autouse=True)
def init_database():
    """Initialize database tables before any tests run."""
    settings = get_settings()
    db_manager = get_db_manager(settings)
    db_manager.create_tables()


@pytest.fixture
def client():
    """Create a test client for the FastAPI app."""
    return TestClient(app)
