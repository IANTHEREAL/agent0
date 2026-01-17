"""Tests for tenant management endpoints."""

import pytest


class TestListTenants:
    """Test GET /api/tenants endpoint."""

    def test_list_tenants_authenticated(self, client, auth_headers):
        """Test listing tenants with valid auth."""
        response = client.get("/api/tenants", headers=auth_headers)
        assert response.status_code == 200
        data = response.json()
        assert isinstance(data, list)

    def test_list_tenants_unauthenticated(self, client):
        """Test listing tenants without auth fails."""
        response = client.get("/api/tenants")
        assert response.status_code == 401


class TestGetTenant:
    """Test GET /api/tenants/{name} endpoint."""

    def test_get_tenant_not_found(self, client, auth_headers):
        """Test getting a non-existent tenant."""
        response = client.get(
            "/api/tenants/nonexistent_tenant_xyz",
            headers=auth_headers
        )
        assert response.status_code == 404

    def test_get_tenant_unauthenticated(self, client):
        """Test getting tenant without auth fails."""
        response = client.get("/api/tenants/test")
        assert response.status_code == 401


class TestCreateTenant:
    """Test POST /api/tenants endpoint."""

    def test_create_tenant_unauthenticated(self, client):
        """Test creating tenant without auth fails."""
        response = client.post(
            "/api/tenants",
            json={"name": "test_tenant"}
        )
        assert response.status_code == 401

    def test_create_tenant_invalid_name(self, client, auth_headers):
        """Test creating tenant with invalid name."""
        response = client.post(
            "/api/tenants",
            json={"name": "INVALID NAME!"},
            headers=auth_headers
        )
        # Should either be 422 (validation) or 400 (bad request)
        assert response.status_code in [400, 422]


class TestDeleteTenant:
    """Test DELETE /api/tenants/{name} endpoint."""

    def test_delete_tenant_unauthenticated(self, client):
        """Test deleting tenant without auth fails."""
        response = client.delete("/api/tenants/test")
        assert response.status_code == 401

    def test_delete_tenant_not_found(self, client, auth_headers):
        """Test deleting non-existent tenant."""
        response = client.delete(
            "/api/tenants/nonexistent_tenant_xyz",
            headers=auth_headers
        )
        # Could be 404 or 200 depending on implementation
        assert response.status_code in [200, 404]
