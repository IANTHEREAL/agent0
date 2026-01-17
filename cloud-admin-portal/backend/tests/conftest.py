"""Pytest fixtures for API tests."""

import os
import pytest
from fastapi.testclient import TestClient

# Set test environment variables before importing app
os.environ.setdefault("PGTIKV_ADMIN_PASSWORD", "testpass")
os.environ.setdefault("PGTIKV_JWT_SECRET", "test-secret-key-for-testing")
os.environ.setdefault("PGTIKV_PD_ENDPOINTS", "127.0.0.1:2379")

from app.main import app


@pytest.fixture
def client():
    """Create a test client for the FastAPI app."""
    return TestClient(app)


@pytest.fixture
def auth_token(client):
    """Get a valid auth token for authenticated requests."""
    response = client.post(
        "/api/auth/login",
        json={"password": "testpass"}
    )
    assert response.status_code == 200
    return response.json()["token"]


@pytest.fixture
def auth_headers(auth_token):
    """Get headers with authorization token."""
    return {"Authorization": f"Bearer {auth_token}"}
