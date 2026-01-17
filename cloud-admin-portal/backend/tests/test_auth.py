"""Tests for authentication endpoints."""

import pytest


class TestLogin:
    """Test /api/auth/login endpoint."""

    def test_login_success(self, client):
        """Test successful login with correct password."""
        response = client.post(
            "/api/auth/login",
            json={"password": "testpass"}
        )
        assert response.status_code == 200
        data = response.json()
        assert "token" in data
        assert "expires_at" in data

    def test_login_wrong_password(self, client):
        """Test login with incorrect password."""
        response = client.post(
            "/api/auth/login",
            json={"password": "wrongpassword"}
        )
        assert response.status_code == 401
        assert "Invalid" in response.json()["detail"]

    def test_login_missing_password(self, client):
        """Test login with missing password field."""
        response = client.post(
            "/api/auth/login",
            json={}
        )
        assert response.status_code == 422  # Validation error


class TestAuthMe:
    """Test /api/auth/me endpoint."""

    def test_me_authenticated(self, client, auth_headers):
        """Test getting current user info with valid token."""
        response = client.get("/api/auth/me", headers=auth_headers)
        assert response.status_code == 200
        data = response.json()
        assert data["user"] == "admin"

    def test_me_unauthenticated(self, client):
        """Test getting current user info without token."""
        response = client.get("/api/auth/me")
        assert response.status_code == 401

    def test_me_invalid_token(self, client):
        """Test getting current user info with invalid token."""
        response = client.get(
            "/api/auth/me",
            headers={"Authorization": "Bearer invalid-token"}
        )
        assert response.status_code == 401
