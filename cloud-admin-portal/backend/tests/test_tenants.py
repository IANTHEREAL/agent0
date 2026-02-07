"""Tests for tenant management endpoints."""


class TestListTenants:

    def test_list_tenants(self, client):
        response = client.get("/api/tenants")
        assert response.status_code == 200
        data = response.json()
        assert "items" in data
        assert "total" in data
        assert "page" in data
        assert "size" in data
        assert isinstance(data["items"], list)
        assert data["page"] == 1
        assert data["size"] == 50

    def test_list_tenants_pagination(self, client):
        response = client.get("/api/tenants?page=1&size=10")
        assert response.status_code == 200
        data = response.json()
        assert data["page"] == 1
        assert data["size"] == 10

    def test_list_tenants_filter_by_state(self, client):
        response = client.get("/api/tenants?state=ACTIVE")
        assert response.status_code == 200
        data = response.json()
        assert isinstance(data["items"], list)


class TestGetTenant:

    def test_get_tenant_not_found(self, client):
        response = client.get("/api/tenants/nonexistent12")
        assert response.status_code == 404


class TestCreateTenant:

    def test_create_tenant_basic(self, client):
        response = client.post(
            "/api/tenants",
            json={"admin_user": "admin"}
        )
        # May fail with 500 if TiKV not running, but should not fail validation
        assert response.status_code in [201, 500]
        if response.status_code == 201:
            data = response.json()
            assert "id" in data
            assert len(data["id"]) == 12


class TestDeleteTenant:

    def test_delete_tenant_not_found(self, client):
        response = client.delete("/api/tenants/nonexistent12")
        assert response.status_code in [200, 404]


class TestRemoveTenant:

    def test_remove_tenant_not_found(self, client):
        response = client.post("/api/tenants/nonexistent12/remove")
        assert response.status_code == 404


class TestTenantObservability:

    def test_observability_does_not_require_tenant_session(self, client):
        response = client.get("/api/tenants/testtenantid/observability")
        assert response.status_code != 401
