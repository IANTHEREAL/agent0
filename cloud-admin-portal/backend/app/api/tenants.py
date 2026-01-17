"""Tenant management API endpoints."""

import secrets
import string
from datetime import datetime, timezone
from typing import List

from fastapi import APIRouter, Depends, HTTPException, status

from ..auth import get_current_user
from ..config import get_settings, Settings
from ..models import (
    TenantCreate,
    TenantResponse,
    TenantCreateResponse,
    TenantConnectRequest,
    TenantConnectResponse,
    MessageResponse,
)
from ..services import PDClient, PgTikvClient
from ..session import session_manager


router = APIRouter()


def get_pd_client(settings: Settings = Depends(get_settings)) -> PDClient:
    """Get PD client instance."""
    return PDClient(settings.pd_endpoints)


def get_pg_client(settings: Settings = Depends(get_settings)) -> PgTikvClient:
    """Get pg-tikv client instance."""
    return PgTikvClient(settings.pg_host, settings.pg_port)


def generate_password(length: int = 16) -> str:
    """Generate a random password."""
    alphabet = string.ascii_letters + string.digits + "!@#$%^&*"
    return "".join(secrets.choice(alphabet) for _ in range(length))


@router.get(
    "",
    response_model=List[TenantResponse],
    summary="List all tenants",
    description="List all tenants (TiKV keyspaces)."
)
async def list_tenants(
    _: str = Depends(get_current_user),
    pd: PDClient = Depends(get_pd_client),
):
    """List all tenants."""
    keyspaces = pd.list_keyspaces()
    tenants = []
    
    for ks in keyspaces:
        if isinstance(ks, dict):
            name = ks.get("name", "")
            state = ks.get("state", "ENABLED")
        else:
            name = str(ks)
            state = "ENABLED"
        
        # Skip system keyspaces
        if name and name != "DEFAULT" and not name.startswith("_"):
            tenants.append(TenantResponse(name=name, state=state))
    
    return tenants


@router.post(
    "",
    response_model=TenantCreateResponse,
    status_code=status.HTTP_201_CREATED,
    summary="Create a new tenant",
    description="""
    Create a new tenant with its own isolated keyspace.
    
    Returns the admin credentials. Save the password - it won't be shown again.
    """
)
async def create_tenant(
    request: TenantCreate,
    _: str = Depends(get_current_user),
    pd: PDClient = Depends(get_pd_client),
    settings: Settings = Depends(get_settings),
):
    """Create a new tenant."""
    # Check if already exists
    if pd.get_keyspace(request.name):
        raise HTTPException(
            status_code=status.HTTP_409_CONFLICT,
            detail=f"Tenant '{request.name}' already exists",
        )
    
    # Create keyspace
    if not pd.create_keyspace(request.name):
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail="Failed to create keyspace in TiKV",
        )
    
    password = request.admin_password or generate_password()
    
    return TenantCreateResponse(
        name=request.name,
        admin_user=request.admin_user,
        admin_password=password,
        connection_string=(
            f"postgresql://{request.name}.{request.admin_user}:{password}"
            f"@{settings.pg_host}:{settings.pg_port}/postgres"
        ),
        created_at=datetime.now(timezone.utc),
    )


@router.get(
    "/{name}",
    response_model=TenantResponse,
    summary="Get tenant details",
    description="Get details for a specific tenant."
)
async def get_tenant(
    name: str,
    _: str = Depends(get_current_user),
    pd: PDClient = Depends(get_pd_client),
    settings: Settings = Depends(get_settings),
):
    """Get tenant details."""
    ks = pd.get_keyspace(name)
    if not ks:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=f"Tenant '{name}' not found",
        )
    
    return TenantResponse(
        name=name,
        state=ks.get("state", "ENABLED") if isinstance(ks, dict) else "ENABLED",
        host=settings.pg_host,
        port=settings.pg_port,
    )


@router.delete(
    "/{name}",
    response_model=MessageResponse,
    summary="Disable a tenant",
    description="""
    Disable a tenant.
    
    Note: TiKV keyspaces cannot be fully deleted, only disabled.
    The data remains but becomes inaccessible.
    """
)
async def delete_tenant(
    name: str,
    _: str = Depends(get_current_user),
    pd: PDClient = Depends(get_pd_client),
):
    """Disable a tenant."""
    if not pd.get_keyspace(name):
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=f"Tenant '{name}' not found",
        )
    
    if not pd.disable_keyspace(name):
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail="Failed to disable tenant",
        )
    
    return MessageResponse(message=f"Tenant '{name}' disabled")


@router.post(
    "/{name}/connect",
    response_model=TenantConnectResponse,
    summary="Connect to tenant for user management",
    description="""
    Validate tenant credentials and create a session for user management.
    
    Returns a session_id that should be included in subsequent user
    management requests as the `X-Tenant-Session` header.
    """
)
async def connect_tenant(
    name: str,
    request: TenantConnectRequest,
    _: str = Depends(get_current_user),
    pd: PDClient = Depends(get_pd_client),
    pg: PgTikvClient = Depends(get_pg_client),
):
    """Validate tenant credentials and create session."""
    # Validate tenant exists
    if not pd.get_keyspace(name):
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=f"Tenant '{name}' not found",
        )
    
    # Validate credentials
    if not pg.test_connection(name, request.admin_user, request.admin_password):
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Invalid tenant credentials",
        )
    
    # Create session
    session = session_manager.create_session(
        tenant_name=name,
        admin_user=request.admin_user,
        admin_password=request.admin_password,
    )
    
    return TenantConnectResponse(
        session_id=session.session_id,
        expires_at=session.expires_at,
    )
