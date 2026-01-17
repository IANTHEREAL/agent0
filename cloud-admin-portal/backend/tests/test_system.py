"""Tests for system endpoints."""

import pytest


class TestHealth:
    """Test /api/health endpoint."""

    def test_health_check(self, client):
        """Test health endpoint returns status."""
        response = client.get("/api/health")
        assert response.status_code == 200
        data = response.json()
        assert "status" in data
        assert data["status"] == "healthy"
        assert "pd_healthy" in data


class TestInfo:
    """Test /api/info endpoint."""

    def test_info(self, client):
        """Test info endpoint returns API information."""
        response = client.get("/api/info")
        assert response.status_code == 200
        data = response.json()
        assert "name" in data
        assert "version" in data
        assert data["name"] == "pg-tikv Admin API"
