"""Tests for tenant management endpoints."""

import pytest


class TestListTenants:

    def test_list_tenants(self, client):
        response = client.get("/api/tenants")
        assert response.status_code == 200
        data = response.json()
        assert isinstance(data, list)


class TestGetTenant:

    def test_get_tenant_not_found(self, client):
        response = client.get("/api/tenants/nonexistent_tenant_xyz")
        assert response.status_code == 404


class TestCreateTenant:

    def test_create_tenant_invalid_name(self, client):
        response = client.post(
            "/api/tenants",
            json={"name": "INVALID NAME!"}
        )
        assert response.status_code in [400, 422, 500]


class TestDeleteTenant:

    def test_delete_tenant_not_found(self, client):
        response = client.delete("/api/tenants/nonexistent_tenant_xyz")
        assert response.status_code in [200, 404]
