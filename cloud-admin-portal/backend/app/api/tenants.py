"""Tenant management API endpoints."""

import secrets
import string
from datetime import datetime, timezone
from typing import List, Optional

from fastapi import APIRouter, Depends, Header, HTTPException, status
from sqlalchemy.orm import Session

from ..config import get_settings, Settings
from ..database import get_db
from ..models import (
    Endpoint,
    EndpointType,
    TenantCreate,
    TenantResponse,
    TenantCreateResponse,
    TenantConnectRequest,
    TenantConnectResponse,
    TenantUpdate,
    TenantResponseExtended,
    MessageResponse,
    SqlQueryRequest,
    SqlQueryResponse,
)
from ..models.db import TenantDB
from ..services import PDClient, PgTikvClient
from ..services.audit import get_audit_service
from ..session import session_manager, TenantSession


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
    response_model=List[TenantResponseExtended],
    summary="List all active tenants",
    description="List all active (non-deleted) tenants with metadata."
)
async def list_tenants(
    pd: PDClient = Depends(get_pd_client),
    db: Session = Depends(get_db),
):
    """List all active tenants."""
    # Get keyspaces from TiKV
    keyspaces = pd.list_keyspaces()
    tikv_keyspaces = {}

    for ks in keyspaces:
        if isinstance(ks, dict):
            name = ks.get("name", "")
            state = ks.get("state", "ENABLED")
        else:
            name = str(ks)
            state = "ENABLED"

        # Skip system keyspaces
        if name and name != "DEFAULT" and not name.startswith("_"):
            tikv_keyspaces[name] = state

    # Get active tenants from database
    active_tenants = db.query(TenantDB).filter(TenantDB.is_deleted == False).all()

    # Build response - only include tenants that exist in both TiKV and DB
    tenants = []
    for tenant_db in active_tenants:
        if tenant_db.name in tikv_keyspaces:
            tenants.append(TenantResponseExtended(
                name=tenant_db.name,
                state=tikv_keyspaces[tenant_db.name],
                is_deleted=tenant_db.is_deleted,
                created_at=tenant_db.created_at,
                created_by=tenant_db.created_by,
                notes=tenant_db.notes,
                tags=tenant_db.tags,
                updated_at=tenant_db.updated_at,
            ))

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
    pd: PDClient = Depends(get_pd_client),
    pg: PgTikvClient = Depends(get_pg_client),
    settings: Settings = Depends(get_settings),
    db: Session = Depends(get_db),
):
    """Create a new tenant."""
    if pd.get_keyspace(request.name):
        raise HTTPException(
            status_code=status.HTTP_409_CONFLICT,
            detail=f"Tenant '{request.name}' already exists",
        )

    existing = db.query(TenantDB).filter(TenantDB.name == request.name).first()
    if existing:
        if existing.is_deleted:
            raise HTTPException(
                status_code=status.HTTP_409_CONFLICT,
                detail=f"Tenant '{request.name}' was previously deleted and cannot be reused",
            )
        else:
            raise HTTPException(
                status_code=status.HTTP_409_CONFLICT,
                detail=f"Tenant '{request.name}' already exists",
            )

    audit = get_audit_service(db)

    try:
        if not pd.create_keyspace(request.name):
            raise HTTPException(
                status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
                detail="Failed to create keyspace in TiKV",
            )

        password = request.admin_password or generate_password()

        if not pg.bootstrap_admin_password(request.name, request.admin_user, password):
            raise HTTPException(
                status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
                detail="Failed to set admin password. Keyspace created but password unchanged.",
            )

        tenant_db = TenantDB(
            name=request.name,
            is_deleted=False,
            created_at=datetime.now(timezone.utc),
            created_by=None,
        )
        db.add(tenant_db)
        db.commit()

        audit.log_tenant_created(request.name, success=True)

        # Use first configured endpoint for connection string
        endpoint_tuples = settings.parse_public_endpoints()
        if endpoint_tuples:
            primary_host, primary_port = endpoint_tuples[0]
        else:
            # Fallback to default if no endpoints configured
            primary_host, primary_port = "127.0.0.1", 5433

        return TenantCreateResponse(
            name=request.name,
            admin_user=request.admin_user,
            admin_password=password,
            connection_string=(
                f"postgresql://{request.name}.{request.admin_user}:{password}"
                f"@{primary_host}:{primary_port}/postgres"
            ),
            created_at=tenant_db.created_at,
        )

    except HTTPException:
        # Log failure and re-raise
        audit.log_tenant_created(request.name, success=False, error="HTTP error during creation")
        raise
    except Exception as e:
        # Log failure
        audit.log_tenant_created(request.name, success=False, error=str(e))
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"Failed to create tenant: {str(e)}",
        )


@router.get(
    "/{name}",
    response_model=TenantResponse,
    summary="Get tenant details",
    description="Get details for a specific tenant."
)
async def get_tenant(
    name: str,
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
    
    # Parse configured endpoints
    endpoint_tuples = settings.parse_public_endpoints()
    endpoints = [
        Endpoint(
            host=host,
            port=port,
            type=EndpointType.LOAD_BALANCER if len(endpoint_tuples) > 1 else EndpointType.PRIMARY,
            priority=100 - i * 10,  # Descending priority based on config order
            description=f"pg-tikv endpoint {i+1}" if len(endpoint_tuples) > 1 else "pg-tikv primary endpoint",
        )
        for i, (host, port) in enumerate(endpoint_tuples)
    ]

    return TenantResponse(
        name=name,
        state=ks.get("state", "ENABLED") if isinstance(ks, dict) else "ENABLED",
        endpoints=endpoints,
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


@router.post(
    "/{name}/remove",
    response_model=MessageResponse,
    summary="Remove tenant from portal (soft delete)",
    description="""
    Remove tenant from the portal UI by marking it as deleted.

    This does NOT delete the TiKV keyspace - it only hides the tenant
    from the portal interface. The keyspace will be disabled in TiKV
    and marked as deleted in the portal database.

    This action cannot be undone and the tenant name cannot be reused.
    """
)
async def remove_tenant(
    name: str,
    pd: PDClient = Depends(get_pd_client),
    db: Session = Depends(get_db),
):
    """Soft delete a tenant."""
    # Check if exists in database
    tenant = db.query(TenantDB).filter(TenantDB.name == name).first()
    if not tenant or tenant.is_deleted:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=f"Tenant '{name}' not found",
        )

    audit = get_audit_service(db)

    try:
        # Disable keyspace in TiKV
        if not pd.disable_keyspace(name):
            raise HTTPException(
                status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
                detail="Failed to disable keyspace in TiKV",
            )

        # Mark as deleted in database
        tenant.is_deleted = True
        tenant.updated_at = datetime.now(timezone.utc)
        db.commit()

        # Log successful deletion
        audit.log_tenant_deleted(name, success=True)

        return MessageResponse(message=f"Tenant '{name}' removed from portal")

    except HTTPException:
        audit.log_tenant_deleted(name, success=False, error="HTTP error during removal")
        raise
    except Exception as e:
        audit.log_tenant_deleted(name, success=False, error=str(e))
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"Failed to remove tenant: {str(e)}",
        )


@router.put(
    "/{name}",
    response_model=TenantResponseExtended,
    summary="Update tenant metadata",
    description="""
    Update tenant notes and tags.

    This endpoint allows you to add organizational metadata to tenants
    without affecting their operational state.
    """
)
async def update_tenant(
    name: str,
    request: TenantUpdate,
    pd: PDClient = Depends(get_pd_client),
    db: Session = Depends(get_db),
):
    """Update tenant metadata."""
    # Check if exists in database
    tenant = db.query(TenantDB).filter(
        TenantDB.name == name,
        TenantDB.is_deleted == False
    ).first()

    if not tenant:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=f"Tenant '{name}' not found",
        )

    audit = get_audit_service(db)

    try:
        # Update fields
        if request.notes is not None:
            tenant.notes = request.notes
        if request.tags is not None:
            tenant.tags = request.tags

        tenant.updated_at = datetime.now(timezone.utc)
        db.commit()
        db.refresh(tenant)

        # Get current state from TiKV
        ks = pd.get_keyspace(name)
        state = ks.get("state", "ENABLED") if isinstance(ks, dict) else "ENABLED"

        # Log update
        audit.log_tenant_updated(
            name,
            success=True,
            extra_metadata={"notes_updated": request.notes is not None, "tags_updated": request.tags is not None}
        )

        return TenantResponseExtended(
            name=tenant.name,
            state=state,
            is_deleted=tenant.is_deleted,
            created_at=tenant.created_at,
            created_by=tenant.created_by,
            notes=tenant.notes,
            tags=tenant.tags,
            updated_at=tenant.updated_at,
        )

    except Exception as e:
        audit.log_tenant_updated(name, success=False, error=str(e))
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"Failed to update tenant: {str(e)}",
        )


async def get_tenant_session(
    name: str,
    x_tenant_session: Optional[str] = Header(None, alias="X-Tenant-Session"),
) -> TenantSession:
    if not x_tenant_session:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Tenant session required. Use POST /api/tenants/{name}/connect first.",
        )
    
    session = session_manager.validate_session(x_tenant_session, name)
    if not session:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Invalid or expired session",
        )
    
    return session


@router.post(
    "/{name}/query",
    response_model=SqlQueryResponse,
    summary="Execute SQL query",
    description="Execute a SQL query on the tenant database. Requires a valid session.",
)
async def execute_query(
    name: str,
    request: SqlQueryRequest,
    session: TenantSession = Depends(get_tenant_session),
    pg: PgTikvClient = Depends(get_pg_client),
):
    sql = request.sql.strip()
    if not sql:
        return SqlQueryResponse(success=False, error="Empty SQL query")
    
    stdout, stderr, rc = pg._run_sql(
        session.tenant_name,
        session.admin_user,
        session.admin_password,
        sql,
    )
    
    if rc != 0:
        return SqlQueryResponse(success=False, error=stderr or "Query execution failed")
    
    return SqlQueryResponse(success=True, result=stdout)
