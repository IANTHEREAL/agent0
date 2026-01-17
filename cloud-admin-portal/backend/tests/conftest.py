"""Pytest fixtures for API tests."""

import os
import pytest
from fastapi.testclient import TestClient

os.environ.setdefault("PGTIKV_PD_ENDPOINTS", "127.0.0.1:2379")

from app.main import app


@pytest.fixture
def client():
    """Create a test client for the FastAPI app."""
    return TestClient(app)
