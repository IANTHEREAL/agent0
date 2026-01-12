#!/usr/bin/env -S uv run
# /// script
# requires-python = ">=3.8"
# dependencies = [
#     "fastapi>=0.104.0",
#     "uvicorn>=0.24.0",
#     "requests>=2.28.0",
#     "pydantic>=2.0.0",
# ]
# ///
"""
pg-tikv Admin REST API Server

RESTful API wrapper for pg-tikv multi-tenant administration.

Usage:
    ./pg_tikv_admin_api.py                    # Start API server on port 8080
    ./pg_tikv_admin_api.py --port 9000        # Custom port
    ./pg_tikv_admin_api.py --host 0.0.0.0     # Listen on all interfaces

API Endpoints:
    GET    /api/tenants              - List all tenants
    POST   /api/tenants              - Create a new tenant
    GET    /api/tenants/{name}       - Get tenant details
    DELETE /api/tenants/{name}       - Delete (disable) a tenant
    
    GET    /api/tenants/{name}/users         - List users in tenant
    POST   /api/tenants/{name}/users         - Create user in tenant
    DELETE /api/tenants/{name}/users/{user}  - Delete user from tenant
    POST   /api/tenants/{name}/users/{user}/reset-password - Reset user password

Environment:
    PD_ENDPOINTS    TiKV PD addresses (default: 127.0.0.1:2379)
    PG_HOST         pg-tikv host (default: 127.0.0.1)
    PG_PORT         pg-tikv port (default: 5433)
    API_PORT        API server port (default: 8080)
"""

import argparse
import os
import sys
from dataclasses import asdict
from typing import Optional, List

from fastapi import FastAPI, HTTPException, Query
from fastapi.middleware.cors import CORSMiddleware
from fastapi.staticfiles import StaticFiles
from fastapi.responses import FileResponse, HTMLResponse
from pydantic import BaseModel, Field
import uvicorn

from pg_tikv_admin import (
    TenantManager,
    generate_password,
    PD_ENDPOINTS,
    PG_HOST,
    PG_PORT,
)


class TenantCreate(BaseModel):
    """Request model for creating a tenant."""
    name: str = Field(..., min_length=3, max_length=64, pattern=r'^[a-z0-9_]+$',
                      description="Tenant name (lowercase alphanumeric with underscores)")
    admin_user: str = Field(default="admin", description="Admin username")
    password: Optional[str] = Field(default=None, description="Admin password (auto-generated if not provided)")


class TenantResponse(BaseModel):
    """Response model for tenant information."""
    name: str
    state: str
    host: Optional[str] = None
    port: Optional[int] = None
    user_format: Optional[str] = None


class TenantCreateResponse(BaseModel):
    """Response model after creating a tenant."""
    tenant: str
    admin_user: str
    password: str
    connection_string: str
    created_at: str


class UserCreate(BaseModel):
    """Request model for creating a user."""
    username: str = Field(..., min_length=1, max_length=64, description="Username to create")
    password: Optional[str] = Field(default=None, description="User password (auto-generated if not provided)")
    superuser: bool = Field(default=False, description="Create as superuser")


class UserResponse(BaseModel):
    """Response model for user information."""
    name: str
    is_superuser: bool
    can_login: bool
    can_create_db: bool
    can_create_role: bool
    roles: List[str] = []


class UserCreateResponse(BaseModel):
    """Response model after creating a user."""
    user: str
    password: str
    connection: str


class PasswordReset(BaseModel):
    """Request model for password reset."""
    new_password: Optional[str] = Field(default=None, description="New password (auto-generated if not provided)")


class PasswordResetResponse(BaseModel):
    """Response model after password reset."""
    user: str
    password: str


class AdminAuth(BaseModel):
    """Admin authentication for operations requiring admin access."""
    admin_user: str = Field(default="admin", description="Admin username")
    admin_password: str = Field(..., description="Admin password")


class ErrorResponse(BaseModel):
    """Error response model."""
    error: str
    detail: Optional[str] = None


app = FastAPI(
    title="pg-tikv Admin API",
    description="RESTful API for pg-tikv multi-tenant administration",
    version="1.0.0",
    docs_url="/api/docs",
    redoc_url="/api/redoc",
)

app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_credentials=True,
    allow_methods=["*"],
    allow_headers=["*"],
)

manager: Optional[TenantManager] = None


def get_manager() -> TenantManager:
    """Get or create the TenantManager instance."""
    global manager
    if manager is None:
        manager = TenantManager(
            os.environ.get("PD_ENDPOINTS", PD_ENDPOINTS),
            os.environ.get("PG_HOST", PG_HOST),
            int(os.environ.get("PG_PORT", PG_PORT)),
        )
    return manager


@app.get("/api/tenants", response_model=List[TenantResponse], tags=["Tenants"])
async def list_tenants():
    """
    List all tenants.
    
    Returns a list of all tenants with their current state.
    """
    mgr = get_manager()
    tenants = mgr.list_tenants()
    return [TenantResponse(name=t["name"], state=t["state"]) for t in tenants]


@app.post("/api/tenants", response_model=TenantCreateResponse, tags=["Tenants"])
async def create_tenant(tenant: TenantCreate):
    """
    Create a new tenant.
    
    Creates a new TiKV keyspace for the tenant and returns connection information.
    """
    mgr = get_manager()
    
    if not mgr._validate_tenant_name(tenant.name):
        raise HTTPException(
            status_code=400,
            detail="Invalid tenant name. Must be 3-64 lowercase alphanumeric characters with underscores."
        )
    
    existing = mgr.get_tenant(tenant.name)
    if existing:
        raise HTTPException(status_code=409, detail=f"Tenant '{tenant.name}' already exists")
    
    if not mgr.pd.create_keyspace(tenant.name):
        raise HTTPException(status_code=500, detail="Failed to create keyspace in TiKV")
    
    password = tenant.password or generate_password()
    
    from datetime import datetime, timezone
    result = TenantCreateResponse(
        tenant=tenant.name,
        admin_user=tenant.admin_user,
        password=password,
        connection_string=f"postgresql://{tenant.name}.{tenant.admin_user}:{password}@{mgr.pg.host}:{mgr.pg.port}/postgres",
        created_at=datetime.now(timezone.utc).isoformat(),
    )
    
    return result


@app.get("/api/tenants/{name}", response_model=TenantResponse, tags=["Tenants"])
async def get_tenant(name: str):
    """
    Get tenant details.
    
    Returns detailed information about a specific tenant including connection info.
    """
    mgr = get_manager()
    tenant = mgr.get_tenant(name)
    
    if not tenant:
        raise HTTPException(status_code=404, detail=f"Tenant '{name}' not found")
    
    return TenantResponse(
        name=tenant["name"],
        state="ENABLED",
        host=tenant["connection_info"]["host"],
        port=tenant["connection_info"]["port"],
        user_format=tenant["connection_info"]["user_format"],
    )


@app.delete("/api/tenants/{name}", tags=["Tenants"])
async def delete_tenant(name: str, force: bool = Query(default=False, description="Skip confirmation")):
    """
    Delete (disable) a tenant.
    
    Note: TiKV keyspaces cannot be fully deleted, only disabled.
    The tenant data remains but becomes inaccessible.
    """
    mgr = get_manager()
    
    existing = mgr.get_tenant(name)
    if not existing:
        raise HTTPException(status_code=404, detail=f"Tenant '{name}' not found")
    
    import requests
    url = f"{mgr.pd.base_url}/pd/api/v2/keyspaces/{name}/state"
    try:
        resp = requests.put(url, json={"state": "DISABLED"}, timeout=10)
        if resp.status_code not in (200, 204):
            raise HTTPException(status_code=500, detail=f"Failed to disable tenant: {resp.text}")
    except requests.RequestException as e:
        raise HTTPException(status_code=500, detail=f"Error connecting to PD: {str(e)}")
    
    return {"message": f"Tenant '{name}' disabled successfully"}


@app.get("/api/tenants/{tenant_name}/users", response_model=List[UserResponse], tags=["Users"])
async def list_users(
    tenant_name: str,
    admin_user: str = Query(default="admin", description="Admin username"),
    admin_password: str = Query(..., description="Admin password"),
):
    """
    List users in a tenant.
    
    Requires admin credentials for the tenant.
    """
    mgr = get_manager()
    
    existing = mgr.get_tenant(tenant_name)
    if not existing:
        raise HTTPException(status_code=404, detail=f"Tenant '{tenant_name}' not found")
    
    users = mgr.list_users(tenant_name, admin_user, admin_password)
    return [
        UserResponse(
            name=u.name,
            is_superuser=u.is_superuser,
            can_login=u.can_login,
            can_create_db=u.can_create_db,
            can_create_role=u.can_create_role,
            roles=u.roles,
        )
        for u in users
    ]


@app.post("/api/tenants/{tenant_name}/users", response_model=UserCreateResponse, tags=["Users"])
async def create_user(
    tenant_name: str,
    user: UserCreate,
    admin_user: str = Query(default="admin", description="Admin username"),
    admin_password: str = Query(..., description="Admin password"),
):
    """
    Create a new user in a tenant.
    
    Requires admin credentials for the tenant.
    """
    mgr = get_manager()
    
    existing = mgr.get_tenant(tenant_name)
    if not existing:
        raise HTTPException(status_code=404, detail=f"Tenant '{tenant_name}' not found")
    
    password = user.password or generate_password()
    
    success = mgr.pg.create_user(
        tenant_name,
        admin_user,
        admin_password,
        user.username,
        password,
        user.superuser,
    )
    
    if not success:
        raise HTTPException(status_code=500, detail="Failed to create user")
    
    return UserCreateResponse(
        user=user.username,
        password=password,
        connection=f"psql -h {mgr.pg.host} -p {mgr.pg.port} -U {tenant_name}.{user.username}",
    )


@app.delete("/api/tenants/{tenant_name}/users/{username}", tags=["Users"])
async def delete_user(
    tenant_name: str,
    username: str,
    admin_user: str = Query(default="admin", description="Admin username"),
    admin_password: str = Query(..., description="Admin password"),
):
    """
    Delete a user from a tenant.
    
    Requires admin credentials for the tenant.
    """
    mgr = get_manager()
    
    existing = mgr.get_tenant(tenant_name)
    if not existing:
        raise HTTPException(status_code=404, detail=f"Tenant '{tenant_name}' not found")
    
    success = mgr.pg.drop_user(tenant_name, admin_user, admin_password, username)
    
    if not success:
        raise HTTPException(status_code=500, detail="Failed to delete user")
    
    return {"message": f"User '{username}' deleted from tenant '{tenant_name}'"}


@app.post("/api/tenants/{tenant_name}/users/{username}/reset-password", 
          response_model=PasswordResetResponse, tags=["Users"])
async def reset_password(
    tenant_name: str,
    username: str,
    reset: PasswordReset,
    admin_user: str = Query(default="admin", description="Admin username"),
    admin_password: str = Query(..., description="Admin password"),
):
    """
    Reset a user's password.
    
    Requires admin credentials for the tenant.
    """
    mgr = get_manager()
    
    existing = mgr.get_tenant(tenant_name)
    if not existing:
        raise HTTPException(status_code=404, detail=f"Tenant '{tenant_name}' not found")
    
    new_password = reset.new_password or generate_password()
    
    success = mgr.pg.reset_password(
        tenant_name,
        admin_user,
        admin_password,
        username,
        new_password,
    )
    
    if not success:
        raise HTTPException(status_code=500, detail="Failed to reset password")
    
    return PasswordResetResponse(user=username, password=new_password)


@app.get("/api/health", tags=["System"])
async def health_check():
    """Health check endpoint."""
    mgr = get_manager()
    
    pd_healthy = False
    try:
        import requests
        resp = requests.get(f"{mgr.pd.base_url}/pd/api/v1/version", timeout=2)
        pd_healthy = resp.status_code == 200
    except:
        pass
    
    return {
        "status": "healthy" if pd_healthy else "degraded",
        "pd_endpoints": mgr.pd.endpoints,
        "pg_host": mgr.pg.host,
        "pg_port": mgr.pg.port,
        "pd_healthy": pd_healthy,
    }


@app.get("/api/info", tags=["System"])
async def api_info():
    """Get API information."""
    return {
        "name": "pg-tikv Admin API",
        "version": "1.0.0",
        "description": "RESTful API for pg-tikv multi-tenant administration",
        "endpoints": {
            "tenants": "/api/tenants",
            "health": "/api/health",
            "docs": "/api/docs",
        },
    }


ADMIN_HTML = """
<!DOCTYPE html>
<html lang="en" class="dark">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>pg-tikv Admin</title>
    <link rel="preconnect" href="https://fonts.googleapis.com">
    <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
    <link href="https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600;700&family=JetBrains+Mono:wght@400;500&display=swap" rel="stylesheet">
    <style>
        :root {
            --background: #09090b;
            --foreground: #fafafa;
            --card: #09090b;
            --card-foreground: #fafafa;
            --popover: #09090b;
            --popover-foreground: #fafafa;
            --primary: #fafafa;
            --primary-foreground: #18181b;
            --secondary: #27272a;
            --secondary-foreground: #fafafa;
            --muted: #27272a;
            --muted-foreground: #a1a1aa;
            --accent: #27272a;
            --accent-foreground: #fafafa;
            --destructive: #7f1d1d;
            --destructive-foreground: #fafafa;
            --border: #27272a;
            --input: #27272a;
            --ring: #d4d4d8;
            --radius: 0.5rem;
        }
        
        * {
            margin: 0;
            padding: 0;
            box-sizing: border-box;
            border-color: var(--border);
        }
        
        body {
            font-family: 'Inter', -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
            background: var(--background);
            color: var(--foreground);
            min-height: 100vh;
            line-height: 1.5;
            -webkit-font-smoothing: antialiased;
        }
        
        .container {
            max-width: 1200px;
            margin: 0 auto;
            padding: 0 24px;
        }
        
        /* Header */
        header {
            border-bottom: 1px solid var(--border);
            padding: 16px 0;
            position: sticky;
            top: 0;
            background: var(--background);
            z-index: 50;
        }
        
        .header-content {
            display: flex;
            justify-content: space-between;
            align-items: center;
        }
        
        .logo {
            display: flex;
            align-items: center;
            gap: 12px;
            font-size: 20px;
            font-weight: 600;
            letter-spacing: -0.025em;
        }
        
        .logo-icon {
            width: 32px;
            height: 32px;
            background: var(--foreground);
            border-radius: 8px;
            display: flex;
            align-items: center;
            justify-content: center;
        }
        
        .logo-icon svg {
            width: 20px;
            height: 20px;
            color: var(--background);
        }
        
        /* Badge */
        .badge {
            display: inline-flex;
            align-items: center;
            gap: 6px;
            padding: 4px 10px;
            font-size: 12px;
            font-weight: 500;
            border-radius: 9999px;
            border: 1px solid var(--border);
            background: var(--secondary);
        }
        
        .badge-dot {
            width: 6px;
            height: 6px;
            border-radius: 50%;
            background: var(--muted-foreground);
        }
        
        .badge-healthy .badge-dot { background: #22c55e; }
        .badge-degraded .badge-dot { background: #eab308; }
        .badge-error .badge-dot { background: #ef4444; }
        
        /* Main */
        main {
            padding: 32px 0;
        }
        
        .page-header {
            margin-bottom: 32px;
        }
        
        .page-title {
            font-size: 30px;
            font-weight: 700;
            letter-spacing: -0.025em;
            margin-bottom: 4px;
        }
        
        .page-description {
            color: var(--muted-foreground);
            font-size: 14px;
        }
        
        /* Card */
        .card {
            background: var(--card);
            border: 1px solid var(--border);
            border-radius: var(--radius);
            overflow: hidden;
        }
        
        .card-header {
            display: flex;
            justify-content: space-between;
            align-items: center;
            padding: 24px;
            border-bottom: 1px solid var(--border);
        }
        
        .card-title {
            font-size: 18px;
            font-weight: 600;
            letter-spacing: -0.025em;
        }
        
        .card-description {
            color: var(--muted-foreground);
            font-size: 14px;
            margin-top: 4px;
        }
        
        .card-content {
            padding: 0;
        }
        
        /* Button */
        .btn {
            display: inline-flex;
            align-items: center;
            justify-content: center;
            gap: 8px;
            font-family: inherit;
            font-size: 14px;
            font-weight: 500;
            padding: 8px 16px;
            border-radius: 6px;
            border: 1px solid transparent;
            cursor: pointer;
            transition: all 150ms ease;
            white-space: nowrap;
        }
        
        .btn:disabled {
            opacity: 0.5;
            cursor: not-allowed;
        }
        
        .btn-primary {
            background: var(--primary);
            color: var(--primary-foreground);
            border-color: var(--primary);
        }
        
        .btn-primary:hover:not(:disabled) {
            opacity: 0.9;
        }
        
        .btn-secondary {
            background: var(--secondary);
            color: var(--secondary-foreground);
            border-color: var(--border);
        }
        
        .btn-secondary:hover:not(:disabled) {
            background: #3f3f46;
        }
        
        .btn-ghost {
            background: transparent;
            color: var(--foreground);
        }
        
        .btn-ghost:hover:not(:disabled) {
            background: var(--accent);
        }
        
        .btn-destructive {
            background: #dc2626;
            color: white;
            border-color: #dc2626;
        }
        
        .btn-destructive:hover:not(:disabled) {
            background: #b91c1c;
        }
        
        .btn-sm {
            padding: 6px 12px;
            font-size: 13px;
        }
        
        .btn-icon {
            padding: 8px;
        }
        
        /* Table */
        table {
            width: 100%;
            border-collapse: collapse;
            font-size: 14px;
        }
        
        th {
            text-align: left;
            padding: 12px 24px;
            font-weight: 500;
            font-size: 12px;
            text-transform: uppercase;
            letter-spacing: 0.05em;
            color: var(--muted-foreground);
            background: var(--muted);
        }
        
        td {
            padding: 16px 24px;
            border-bottom: 1px solid var(--border);
        }
        
        tr:last-child td {
            border-bottom: none;
        }
        
        tr:hover td {
            background: var(--muted);
        }
        
        .table-cell-name {
            font-weight: 500;
            cursor: pointer;
        }
        
        .table-cell-name:hover {
            text-decoration: underline;
        }
        
        .table-actions {
            display: flex;
            gap: 8px;
            justify-content: flex-end;
        }
        
        /* Status */
        .status {
            display: inline-flex;
            align-items: center;
            gap: 6px;
            font-size: 13px;
        }
        
        .status-dot {
            width: 8px;
            height: 8px;
            border-radius: 50%;
        }
        
        .status-enabled .status-dot { background: #22c55e; }
        .status-disabled .status-dot { background: #ef4444; }
        
        /* Code */
        code {
            font-family: 'JetBrains Mono', monospace;
            font-size: 13px;
            padding: 2px 6px;
            background: var(--muted);
            border-radius: 4px;
        }
        
        /* Empty State */
        .empty-state {
            padding: 64px 24px;
            text-align: center;
        }
        
        .empty-icon {
            width: 48px;
            height: 48px;
            margin: 0 auto 16px;
            color: var(--muted-foreground);
        }
        
        .empty-title {
            font-size: 16px;
            font-weight: 500;
            margin-bottom: 4px;
        }
        
        .empty-description {
            color: var(--muted-foreground);
            font-size: 14px;
        }
        
        /* Loading */
        .loading {
            padding: 64px 24px;
            text-align: center;
            color: var(--muted-foreground);
        }
        
        .spinner {
            width: 24px;
            height: 24px;
            border: 2px solid var(--border);
            border-top-color: var(--foreground);
            border-radius: 50%;
            animation: spin 0.8s linear infinite;
            margin: 0 auto 12px;
        }
        
        @keyframes spin {
            to { transform: rotate(360deg); }
        }
        
        /* Modal */
        .modal-overlay {
            display: none;
            position: fixed;
            inset: 0;
            background: rgba(0, 0, 0, 0.8);
            backdrop-filter: blur(4px);
            z-index: 100;
            align-items: center;
            justify-content: center;
            padding: 24px;
        }
        
        .modal-overlay.active {
            display: flex;
        }
        
        .modal {
            background: var(--card);
            border: 1px solid var(--border);
            border-radius: 12px;
            width: 100%;
            max-width: 440px;
            box-shadow: 0 25px 50px -12px rgba(0, 0, 0, 0.5);
            animation: modal-in 200ms ease;
        }
        
        @keyframes modal-in {
            from {
                opacity: 0;
                transform: scale(0.95) translateY(10px);
            }
            to {
                opacity: 1;
                transform: scale(1) translateY(0);
            }
        }
        
        .modal-header {
            padding: 24px 24px 0;
        }
        
        .modal-title {
            font-size: 18px;
            font-weight: 600;
            letter-spacing: -0.025em;
        }
        
        .modal-description {
            color: var(--muted-foreground);
            font-size: 14px;
            margin-top: 4px;
        }
        
        .modal-content {
            padding: 24px;
        }
        
        .modal-footer {
            padding: 0 24px 24px;
            display: flex;
            gap: 12px;
            justify-content: flex-end;
        }
        
        /* Form */
        .form-group {
            margin-bottom: 20px;
        }
        
        .form-group:last-child {
            margin-bottom: 0;
        }
        
        .form-label {
            display: block;
            font-size: 14px;
            font-weight: 500;
            margin-bottom: 8px;
        }
        
        .form-hint {
            font-size: 12px;
            color: var(--muted-foreground);
            margin-top: 6px;
        }
        
        .form-input {
            width: 100%;
            padding: 10px 12px;
            font-family: inherit;
            font-size: 14px;
            background: var(--background);
            border: 1px solid var(--border);
            border-radius: 6px;
            color: var(--foreground);
            transition: border-color 150ms ease, box-shadow 150ms ease;
        }
        
        .form-input::placeholder {
            color: var(--muted-foreground);
        }
        
        .form-input:focus {
            outline: none;
            border-color: var(--ring);
            box-shadow: 0 0 0 3px rgba(212, 212, 216, 0.1);
        }
        
        .form-row {
            display: grid;
            grid-template-columns: 1fr 1fr;
            gap: 16px;
        }
        
        .form-checkbox {
            display: flex;
            align-items: center;
            gap: 10px;
            cursor: pointer;
        }
        
        .form-checkbox input[type="checkbox"] {
            width: 16px;
            height: 16px;
            accent-color: var(--primary);
            cursor: pointer;
        }
        
        /* Alert */
        .alert-container {
            position: fixed;
            top: 80px;
            right: 24px;
            z-index: 200;
            display: flex;
            flex-direction: column;
            gap: 8px;
        }
        
        .alert {
            padding: 12px 16px;
            border-radius: 8px;
            font-size: 14px;
            display: flex;
            align-items: center;
            gap: 10px;
            animation: alert-in 200ms ease;
            border: 1px solid var(--border);
            background: var(--card);
            min-width: 300px;
            box-shadow: 0 10px 15px -3px rgba(0, 0, 0, 0.3);
        }
        
        @keyframes alert-in {
            from {
                opacity: 0;
                transform: translateX(20px);
            }
            to {
                opacity: 1;
                transform: translateX(0);
            }
        }
        
        .alert-success {
            border-color: #166534;
            background: #14532d;
        }
        
        .alert-error {
            border-color: #991b1b;
            background: #7f1d1d;
        }
        
        /* Connection Info */
        .connection-box {
            background: var(--muted);
            border: 1px solid var(--border);
            border-radius: 8px;
            padding: 16px;
            margin-top: 16px;
        }
        
        .connection-box code {
            display: block;
            padding: 12px;
            background: var(--background);
            border-radius: 6px;
            font-size: 12px;
            word-break: break-all;
            margin-top: 8px;
        }
        
        .info-row {
            display: flex;
            justify-content: space-between;
            padding: 8px 0;
            border-bottom: 1px solid var(--border);
        }
        
        .info-row:last-child {
            border-bottom: none;
        }
        
        .info-label {
            color: var(--muted-foreground);
            font-size: 13px;
        }
        
        .info-value {
            font-weight: 500;
            font-size: 13px;
        }
        
        .warning-text {
            display: flex;
            align-items: center;
            gap: 8px;
            padding: 12px;
            background: #422006;
            border: 1px solid #854d0e;
            border-radius: 6px;
            font-size: 13px;
            color: #fef08a;
            margin-top: 16px;
        }
        
        /* Users Section */
        .users-section {
            margin-top: 24px;
        }
        
        .auth-bar {
            display: flex;
            gap: 12px;
            padding: 20px 24px;
            background: var(--muted);
            border-bottom: 1px solid var(--border);
            align-items: flex-end;
        }
        
        .auth-bar .form-group {
            flex: 1;
            margin-bottom: 0;
        }
        
        .back-link {
            display: inline-flex;
            align-items: center;
            gap: 6px;
            color: var(--muted-foreground);
            font-size: 14px;
            text-decoration: none;
            margin-bottom: 16px;
            cursor: pointer;
        }
        
        .back-link:hover {
            color: var(--foreground);
        }
        
        .hidden {
            display: none !important;
        }
        
        /* SVG Icons */
        .icon {
            width: 16px;
            height: 16px;
            flex-shrink: 0;
        }
        
        .icon-lg {
            width: 20px;
            height: 20px;
        }
    </style>
</head>
<body>
    <header>
        <div class="container">
            <div class="header-content">
                <div class="logo">
                    <div class="logo-icon">
                        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                            <path d="M12 2L2 7l10 5 10-5-10-5z"/>
                            <path d="M2 17l10 5 10-5"/>
                            <path d="M2 12l10 5 10-5"/>
                        </svg>
                    </div>
                    pg-tikv
                </div>
                <div id="healthStatus" class="badge">
                    <span class="badge-dot"></span>
                    <span>Checking...</span>
                </div>
            </div>
        </div>
    </header>
    
    <main>
        <div class="container">
            <div class="page-header">
                <h1 class="page-title">Tenants</h1>
                <p class="page-description">Manage your multi-tenant database instances</p>
            </div>
            
            <div id="alertContainer" class="alert-container"></div>
            
            <div class="card">
                <div class="card-header">
                    <div>
                        <h2 class="card-title">All Tenants</h2>
                        <p class="card-description">View and manage tenant keyspaces</p>
                    </div>
                    <button class="btn btn-primary" onclick="showCreateTenantModal()">
                        <svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                            <path d="M12 5v14M5 12h14"/>
                        </svg>
                        New Tenant
                    </button>
                </div>
                <div class="card-content">
                    <div id="tenantsLoading" class="loading">
                        <div class="spinner"></div>
                        Loading tenants...
                    </div>
                    <table id="tenantsTable" class="hidden">
                        <thead>
                            <tr>
                                <th>Name</th>
                                <th>Status</th>
                                <th>Connection</th>
                                <th style="text-align: right;">Actions</th>
                            </tr>
                        </thead>
                        <tbody id="tenantsBody"></tbody>
                    </table>
                    <div id="tenantsEmpty" class="empty-state hidden">
                        <svg class="empty-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5">
                            <path d="M20 7l-8-4-8 4m16 0l-8 4m8-4v10l-8 4m0-10L4 7m8 4v10M4 7v10l8 4"/>
                        </svg>
                        <p class="empty-title">No tenants yet</p>
                        <p class="empty-description">Create your first tenant to get started</p>
                    </div>
                </div>
            </div>
            
            <div id="usersSection" class="card users-section hidden">
                <div class="card-header">
                    <div>
                        <span class="back-link" onclick="hideUsersSection()">
                            <svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                                <path d="M19 12H5M12 19l-7-7 7-7"/>
                            </svg>
                            Back to tenants
                        </span>
                        <h2 class="card-title">Users in <span id="currentTenantName"></span></h2>
                        <p class="card-description">Manage database users for this tenant</p>
                    </div>
                    <button class="btn btn-primary btn-sm" onclick="showCreateUserModal()">
                        <svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                            <path d="M12 5v14M5 12h14"/>
                        </svg>
                        New User
                    </button>
                </div>
                <div class="auth-bar">
                    <div class="form-group">
                        <label class="form-label">Admin User</label>
                        <input type="text" id="adminUser" class="form-input" value="admin">
                    </div>
                    <div class="form-group">
                        <label class="form-label">Admin Password</label>
                        <input type="password" id="adminPassword" class="form-input" value="admin" placeholder="Default: admin">
                    </div>
                </div>
                <div class="card-content" style="padding: 24px;">
                    <div class="info-box" style="background: var(--muted); border-radius: 8px; padding: 16px; margin-bottom: 16px;">
                        <p style="color: var(--muted-foreground); font-size: 13px; margin-bottom: 8px;">
                            <strong>Note:</strong> User listing requires pg_roles support. Use the buttons below to manage users.
                        </p>
                        <p style="color: var(--muted-foreground); font-size: 13px;">
                            Default admin password is <code>admin</code>. Connection format: <code id="connFormat"></code>
                        </p>
                    </div>
                    <div style="display: flex; gap: 12px; flex-wrap: wrap;">
                        <button class="btn btn-secondary" onclick="testConnection()">
                            <svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                                <path d="M22 11.08V12a10 10 0 1 1-5.93-9.14"/>
                                <polyline points="22 4 12 14.01 9 11.01"/>
                            </svg>
                            Test Connection
                        </button>
                        <button class="btn btn-secondary" onclick="showResetPasswordModal()">
                            <svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                                <rect x="3" y="11" width="18" height="11" rx="2" ry="2"/>
                                <path d="M7 11V7a5 5 0 0 1 10 0v4"/>
                            </svg>
                            Reset Password
                        </button>
                        <button class="btn btn-secondary" onclick="showDeleteUserModal()">
                            <svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                                <path d="M16 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2"/>
                                <circle cx="8.5" cy="7" r="4"/>
                                <line x1="18" y1="8" x2="23" y2="13"/>
                                <line x1="23" y1="8" x2="18" y2="13"/>
                            </svg>
                            Delete User
                        </button>
                    </div>
                </div>
            </div>
        </div>
    </main>
    
    <!-- Create Tenant Modal -->
    <div id="createTenantModal" class="modal-overlay">
        <div class="modal">
            <div class="modal-header">
                <h3 class="modal-title">Create Tenant</h3>
                <p class="modal-description">Create a new isolated database tenant</p>
            </div>
            <form id="createTenantForm" onsubmit="createTenant(event)">
                <div class="modal-content">
                    <div class="form-group">
                        <label class="form-label">Tenant Name</label>
                        <input type="text" id="tenantName" class="form-input" 
                               pattern="[a-z0-9_]+" minlength="3" maxlength="64" 
                               placeholder="acme_corp" required>
                        <p class="form-hint">Lowercase letters, numbers, and underscores only (3-64 chars)</p>
                    </div>
                    <div class="form-row">
                        <div class="form-group">
                            <label class="form-label">Admin User</label>
                            <input type="text" id="newAdminUser" class="form-input" value="admin">
                        </div>
                        <div class="form-group">
                            <label class="form-label">Password</label>
                            <input type="text" id="newAdminPassword" class="form-input" placeholder="Auto-generate">
                        </div>
                    </div>
                </div>
                <div class="modal-footer">
                    <button type="button" class="btn btn-secondary" onclick="hideCreateTenantModal()">Cancel</button>
                    <button type="submit" class="btn btn-primary">Create Tenant</button>
                </div>
            </form>
        </div>
    </div>
    
    <!-- Create User Modal -->
    <div id="createUserModal" class="modal-overlay">
        <div class="modal">
            <div class="modal-header">
                <h3 class="modal-title">Create User</h3>
                <p class="modal-description">Add a new user to this tenant</p>
            </div>
            <form id="createUserForm" onsubmit="createUser(event)">
                <div class="modal-content">
                    <div class="form-group">
                        <label class="form-label">Username</label>
                        <input type="text" id="newUsername" class="form-input" placeholder="developer" required>
                    </div>
                    <div class="form-group">
                        <label class="form-label">Password</label>
                        <input type="text" id="newUserPassword" class="form-input" placeholder="Auto-generate if empty">
                    </div>
                    <div class="form-group">
                        <label class="form-checkbox">
                            <input type="checkbox" id="newUserSuperuser">
                            <span>Grant superuser privileges</span>
                        </label>
                    </div>
                </div>
                <div class="modal-footer">
                    <button type="button" class="btn btn-secondary" onclick="hideCreateUserModal()">Cancel</button>
                    <button type="submit" class="btn btn-primary">Create User</button>
                </div>
            </form>
        </div>
    </div>
    
    <!-- Result Modal -->
    <div id="resultModal" class="modal-overlay">
        <div class="modal">
            <div class="modal-header">
                <h3 class="modal-title" id="resultTitle">Success</h3>
            </div>
            <div class="modal-content" id="resultContent"></div>
            <div class="modal-footer">
                <button class="btn btn-primary" onclick="hideResultModal()">Done</button>
            </div>
        </div>
    </div>

    <script>
        const API_BASE = '/api';
        let currentTenant = null;
        
        document.addEventListener('DOMContentLoaded', () => {
            checkHealth();
            loadTenants();
        });
        
        async function checkHealth() {
            try {
                const res = await fetch(`${API_BASE}/health`);
                const data = await res.json();
                const badge = document.getElementById('healthStatus');
                if (data.pd_healthy) {
                    badge.innerHTML = '<span class="badge-dot"></span><span>Connected</span>';
                    badge.className = 'badge badge-healthy';
                } else {
                    badge.innerHTML = '<span class="badge-dot"></span><span>Degraded</span>';
                    badge.className = 'badge badge-degraded';
                }
            } catch (e) {
                const badge = document.getElementById('healthStatus');
                badge.innerHTML = '<span class="badge-dot"></span><span>Error</span>';
                badge.className = 'badge badge-error';
            }
        }
        
        function showAlert(message, type = 'success') {
            const container = document.getElementById('alertContainer');
            const alert = document.createElement('div');
            alert.className = `alert alert-${type}`;
            const icon = type === 'success' 
                ? '<svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M22 11.08V12a10 10 0 1 1-5.93-9.14"/><polyline points="22 4 12 14.01 9 11.01"/></svg>'
                : '<svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="12" cy="12" r="10"/><line x1="12" y1="8" x2="12" y2="12"/><line x1="12" y1="16" x2="12.01" y2="16"/></svg>';
            alert.innerHTML = icon + '<span>' + message + '</span>';
            container.appendChild(alert);
            setTimeout(() => alert.remove(), 4000);
        }
        
        async function loadTenants() {
            const loading = document.getElementById('tenantsLoading');
            const table = document.getElementById('tenantsTable');
            const empty = document.getElementById('tenantsEmpty');
            const tbody = document.getElementById('tenantsBody');
            
            loading.classList.remove('hidden');
            table.classList.add('hidden');
            empty.classList.add('hidden');
            
            try {
                const res = await fetch(`${API_BASE}/tenants`);
                const tenants = await res.json();
                
                loading.classList.add('hidden');
                
                if (tenants.length === 0) {
                    empty.classList.remove('hidden');
                    return;
                }
                
                tbody.innerHTML = tenants.map(t => `
                    <tr>
                        <td class="table-cell-name" onclick="showTenantUsers('${t.name}')">${t.name}</td>
                        <td>
                            <span class="status status-${t.state.toLowerCase()}">
                                <span class="status-dot"></span>
                                ${t.state}
                            </span>
                        </td>
                        <td><code>${t.name}.&lt;user&gt;</code></td>
                        <td class="table-actions">
                            <button class="btn btn-ghost btn-sm" onclick="showTenantUsers('${t.name}')">Users</button>
                            <button class="btn btn-ghost btn-sm" onclick="deleteTenant('${t.name}')" style="color: #ef4444;">Delete</button>
                        </td>
                    </tr>
                `).join('');
                
                table.classList.remove('hidden');
            } catch (e) {
                loading.classList.add('hidden');
                showAlert('Failed to load tenants', 'error');
            }
        }
        
        function generateTenantName() {
            const chars = 'abcdefghijklmnopqrstuvwxyz';
            let result = '';
            for (let i = 0; i < 10; i++) {
                result += chars.charAt(Math.floor(Math.random() * chars.length));
            }
            return result;
        }
        
        function showCreateTenantModal() {
            document.getElementById('tenantName').value = generateTenantName();
            document.getElementById('createTenantModal').classList.add('active');
            document.getElementById('tenantName').focus();
            document.getElementById('tenantName').select();
        }
        
        function hideCreateTenantModal() {
            document.getElementById('createTenantModal').classList.remove('active');
            document.getElementById('createTenantForm').reset();
        }
        
        async function createTenant(e) {
            e.preventDefault();
            
            const name = document.getElementById('tenantName').value;
            const adminUser = document.getElementById('newAdminUser').value || 'admin';
            const password = document.getElementById('newAdminPassword').value || null;
            
            try {
                const res = await fetch(`${API_BASE}/tenants`, {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({ name, admin_user: adminUser, password })
                });
                
                if (!res.ok) {
                    const err = await res.json();
                    throw new Error(err.detail || 'Failed to create tenant');
                }
                
                const data = await res.json();
                hideCreateTenantModal();
                loadTenants();
                
                document.getElementById('resultTitle').textContent = 'Tenant Created';
                document.getElementById('resultContent').innerHTML = `
                    <div class="info-row">
                        <span class="info-label">Tenant</span>
                        <span class="info-value">${data.tenant}</span>
                    </div>
                    <div class="info-row">
                        <span class="info-label">Admin User</span>
                        <span class="info-value">${data.admin_user}</span>
                    </div>
                    <div class="info-row">
                        <span class="info-label">Password</span>
                        <span class="info-value"><code>${data.password}</code></span>
                    </div>
                    <div class="connection-box">
                        <span class="info-label">Connection String</span>
                        <code>${data.connection_string}</code>
                    </div>
                    <div class="warning-text">
                        <svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                            <path d="M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z"/>
                            <line x1="12" y1="9" x2="12" y2="13"/>
                            <line x1="12" y1="17" x2="12.01" y2="17"/>
                        </svg>
                        Save these credentials. The password won't be shown again.
                    </div>
                `;
                document.getElementById('resultModal').classList.add('active');
            } catch (e) {
                showAlert(e.message, 'error');
            }
        }
        
        async function deleteTenant(name) {
            if (!confirm('Delete tenant "' + name + '"? This will disable the keyspace.')) return;
            
            try {
                const res = await fetch(`${API_BASE}/tenants/${name}?force=true`, { method: 'DELETE' });
                if (!res.ok) throw new Error('Failed to delete tenant');
                showAlert('Tenant disabled successfully');
                loadTenants();
            } catch (e) {
                showAlert(e.message, 'error');
            }
        }
        
        function showTenantUsers(tenantName) {
            currentTenant = tenantName;
            document.getElementById('currentTenantName').textContent = tenantName;
            document.getElementById('usersSection').classList.remove('hidden');
            document.getElementById('usersTable').classList.add('hidden');
            document.getElementById('usersEmpty').classList.remove('hidden');
            document.getElementById('adminPassword').value = '';
            document.getElementById('adminPassword').focus();
        }
        
        function hideUsersSection() {
            currentTenant = null;
            document.getElementById('usersSection').classList.add('hidden');
        }
        
        async function loadUsers() {
            const adminUser = document.getElementById('adminUser').value;
            const adminPassword = document.getElementById('adminPassword').value;
            
            if (!adminPassword) {
                showAlert('Enter admin password', 'error');
                return;
            }
            
            const loading = document.getElementById('usersLoading');
            const table = document.getElementById('usersTable');
            const empty = document.getElementById('usersEmpty');
            const tbody = document.getElementById('usersBody');
            
            loading.classList.remove('hidden');
            table.classList.add('hidden');
            empty.classList.add('hidden');
            
            try {
                const params = new URLSearchParams({ admin_user: adminUser, admin_password: adminPassword });
                const res = await fetch(`${API_BASE}/tenants/${currentTenant}/users?${params}`);
                
                if (!res.ok) throw new Error('Authentication failed');
                
                const users = await res.json();
                loading.classList.add('hidden');
                
                if (users.length === 0) {
                    empty.classList.remove('hidden');
                    return;
                }
                
                tbody.innerHTML = users.map(u => `
                    <tr>
                        <td style="font-weight: 500;">${u.name}</td>
                        <td>${u.is_superuser ? '✓' : '—'}</td>
                        <td>${u.can_login ? '✓' : '—'}</td>
                        <td>${u.can_create_db ? '✓' : '—'}</td>
                        <td class="table-actions">
                            <button class="btn btn-ghost btn-sm" onclick="resetUserPassword('${u.name}')">Reset</button>
                            <button class="btn btn-ghost btn-sm" onclick="deleteUser('${u.name}')" style="color: #ef4444;">Delete</button>
                        </td>
                    </tr>
                `).join('');
                
                table.classList.remove('hidden');
            } catch (e) {
                loading.classList.add('hidden');
                empty.classList.remove('hidden');
                showAlert(e.message, 'error');
            }
        }
        
        function showCreateUserModal() {
            if (!currentTenant) return;
            document.getElementById('createUserModal').classList.add('active');
            document.getElementById('newUsername').focus();
        }
        
        function hideCreateUserModal() {
            document.getElementById('createUserModal').classList.remove('active');
            document.getElementById('createUserForm').reset();
        }
        
        async function createUser(e) {
            e.preventDefault();
            
            const username = document.getElementById('newUsername').value;
            const password = document.getElementById('newUserPassword').value || null;
            const superuser = document.getElementById('newUserSuperuser').checked;
            const adminUser = document.getElementById('adminUser').value;
            const adminPassword = document.getElementById('adminPassword').value;
            
            if (!adminPassword) {
                showAlert('Authenticate first', 'error');
                return;
            }
            
            try {
                const params = new URLSearchParams({ admin_user: adminUser, admin_password: adminPassword });
                const res = await fetch(`${API_BASE}/tenants/${currentTenant}/users?${params}`, {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({ username, password, superuser })
                });
                
                if (!res.ok) throw new Error('Failed to create user');
                
                const data = await res.json();
                hideCreateUserModal();
                loadUsers();
                
                document.getElementById('resultTitle').textContent = 'User Created';
                document.getElementById('resultContent').innerHTML = `
                    <div class="info-row">
                        <span class="info-label">Username</span>
                        <span class="info-value">${data.user}</span>
                    </div>
                    <div class="info-row">
                        <span class="info-label">Password</span>
                        <span class="info-value"><code>${data.password}</code></span>
                    </div>
                    <div class="connection-box">
                        <span class="info-label">Connection</span>
                        <code>${data.connection}</code>
                    </div>
                    <div class="warning-text">
                        <svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                            <path d="M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z"/>
                            <line x1="12" y1="9" x2="12" y2="13"/>
                            <line x1="12" y1="17" x2="12.01" y2="17"/>
                        </svg>
                        Save these credentials. The password won't be shown again.
                    </div>
                `;
                document.getElementById('resultModal').classList.add('active');
            } catch (e) {
                showAlert(e.message, 'error');
            }
        }
        
        async function resetUserPassword(username) {
            const adminUser = document.getElementById('adminUser').value;
            const adminPassword = document.getElementById('adminPassword').value;
            
            if (!adminPassword || !confirm('Reset password for "' + username + '"?')) return;
            
            try {
                const params = new URLSearchParams({ admin_user: adminUser, admin_password: adminPassword });
                const res = await fetch(`${API_BASE}/tenants/${currentTenant}/users/${username}/reset-password?${params}`, {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({})
                });
                
                if (!res.ok) throw new Error('Failed to reset password');
                
                const data = await res.json();
                
                document.getElementById('resultTitle').textContent = 'Password Reset';
                document.getElementById('resultContent').innerHTML = `
                    <div class="info-row">
                        <span class="info-label">User</span>
                        <span class="info-value">${data.user}</span>
                    </div>
                    <div class="info-row">
                        <span class="info-label">New Password</span>
                        <span class="info-value"><code>${data.password}</code></span>
                    </div>
                    <div class="warning-text">
                        <svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                            <path d="M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z"/>
                            <line x1="12" y1="9" x2="12" y2="13"/>
                            <line x1="12" y1="17" x2="12.01" y2="17"/>
                        </svg>
                        Save the new password.
                    </div>
                `;
                document.getElementById('resultModal').classList.add('active');
            } catch (e) {
                showAlert(e.message, 'error');
            }
        }
        
        async function deleteUser(username) {
            const adminUser = document.getElementById('adminUser').value;
            const adminPassword = document.getElementById('adminPassword').value;
            
            if (!adminPassword || !confirm('Delete user "' + username + '"?')) return;
            
            try {
                const params = new URLSearchParams({ admin_user: adminUser, admin_password: adminPassword });
                const res = await fetch(`${API_BASE}/tenants/${currentTenant}/users/${username}?${params}`, { method: 'DELETE' });
                if (!res.ok) throw new Error('Failed to delete user');
                showAlert('User deleted');
                loadUsers();
            } catch (e) {
                showAlert(e.message, 'error');
            }
        }
        
        function hideResultModal() {
            document.getElementById('resultModal').classList.remove('active');
        }
        
        document.querySelectorAll('.modal-overlay').forEach(overlay => {
            overlay.addEventListener('click', e => {
                if (e.target === overlay) overlay.classList.remove('active');
            });
        });
    </script>
</body>
</html>
"""


@app.get("/", response_class=HTMLResponse)
async def admin_ui():
    """Serve the admin frontend."""
    return HTMLResponse(content=ADMIN_HTML)


def main():
    parser = argparse.ArgumentParser(
        description="pg-tikv Admin REST API Server",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Environment Variables:
  PD_ENDPOINTS    TiKV PD addresses (default: 127.0.0.1:2379)
  PG_HOST         pg-tikv host (default: 127.0.0.1)
  PG_PORT         pg-tikv port (default: 5433)
  API_PORT        API server port (default: 8080)

Examples:
  # Start API server
  ./pg_tikv_admin_api.py

  # Custom port
  ./pg_tikv_admin_api.py --port 9000

  # Listen on all interfaces
  ./pg_tikv_admin_api.py --host 0.0.0.0 --port 8080
""",
    )
    
    parser.add_argument("--host", default="127.0.0.1", help="Host to bind (default: 127.0.0.1)")
    parser.add_argument("--port", type=int, default=int(os.environ.get("API_PORT", "8080")), 
                        help="Port to listen on (default: 8080)")
    parser.add_argument("--reload", action="store_true", help="Enable auto-reload for development")
    
    args = parser.parse_args()
    
    print(f"""
╔═══════════════════════════════════════════════════════════════╗
║              pg-tikv Admin API Server                         ║
╠═══════════════════════════════════════════════════════════════╣
║  API URL:     http://{args.host}:{args.port}/api              
║  Admin UI:    http://{args.host}:{args.port}/                 
║  API Docs:    http://{args.host}:{args.port}/api/docs         
║                                                               ║
║  PD Endpoints: {os.environ.get("PD_ENDPOINTS", PD_ENDPOINTS):<42}║
║  PG Host:      {os.environ.get("PG_HOST", PG_HOST):<42}║
║  PG Port:      {os.environ.get("PG_PORT", str(PG_PORT)):<42}║
╚═══════════════════════════════════════════════════════════════╝
""")
    
    uvicorn.run(
        "pg_tikv_admin_api:app" if args.reload else app,
        host=args.host,
        port=args.port,
        reload=args.reload,
    )


if __name__ == "__main__":
    main()
