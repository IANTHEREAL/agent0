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
    TenantObservabilityResponse,
)
from ..models.db import TenantDB
from ..services import PDClient, PgTikvClient, generate_tenant_id, make_keyspace, is_tenant_keyspace, parse_keyspace
from ..services.audit import get_audit_service
from ..session import session_manager, TenantSession


router = APIRouter()

OBSERVABILITY_USER = "_pgtikv_sys_observer"


def get_pd_client(settings: Settings = Depends(get_settings)) -> PDClient:
    return PDClient(settings.pd_endpoints)


def get_pg_client(settings: Settings = Depends(get_settings)) -> PgTikvClient:
    return PgTikvClient(settings.pg_host, settings.pg_port)


def generate_password(length: int = 16) -> str:
    alphabet = string.ascii_letters + string.digits + "!@#$%^&*"
    return "".join(secrets.choice(alphabet) for _ in range(length))


def get_tenant_or_404(tenant_id: str, db: Session) -> TenantDB:
    tenant = db.query(TenantDB).filter(
        TenantDB.id == tenant_id,
        TenantDB.is_deleted == False
    ).first()
    if not tenant:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=f"Tenant '{tenant_id}' not found",
        )
    return tenant


@router.get(
    "",
    response_model=List[TenantResponseExtended],
    summary="List all active tenants",
)
async def list_tenants(
    pd: PDClient = Depends(get_pd_client),
    db: Session = Depends(get_db),
):
    keyspaces = pd.list_keyspaces()
    tikv_keyspaces = {}

    for ks in keyspaces:
        if isinstance(ks, dict):
            name = ks.get("name", "")
            state = ks.get("state", "ENABLED")
        else:
            name = str(ks)
            state = "ENABLED"

        if name and is_tenant_keyspace(name):
            tikv_keyspaces[name] = state

    active_tenants = db.query(TenantDB).filter(TenantDB.is_deleted == False).all()

    tenants = []
    for tenant_db in active_tenants:
        if tenant_db.keyspace in tikv_keyspaces:
            tenants.append(TenantResponseExtended(
                id=tenant_db.id,
                state=tikv_keyspaces[tenant_db.keyspace],
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
)
async def create_tenant(
    request: TenantCreate,
    pd: PDClient = Depends(get_pd_client),
    pg: PgTikvClient = Depends(get_pg_client),
    settings: Settings = Depends(get_settings),
    db: Session = Depends(get_db),
):
    tenant_id = generate_tenant_id()
    keyspace = make_keyspace(tenant_id)

    existing = db.query(TenantDB).filter(TenantDB.id == tenant_id).first()
    if existing:
        raise HTTPException(
            status_code=status.HTTP_409_CONFLICT,
            detail="ID collision, please retry",
        )

    audit = get_audit_service(db)

    try:
        if not pd.create_keyspace(keyspace):
            raise HTTPException(
                status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
                detail="Failed to create keyspace in TiKV",
            )

        password = request.admin_password or generate_password()

        if not pg.bootstrap_admin_password(keyspace, request.admin_user, password):
            raise HTTPException(
                status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
                detail="Failed to set admin password. Keyspace created but password unchanged.",
            )

        tenant_db = TenantDB(
            id=tenant_id,
            keyspace=keyspace,
            is_deleted=False,
            created_at=datetime.now(timezone.utc),
            created_by=None,
        )
        db.add(tenant_db)
        db.commit()

        audit.log_tenant_created(tenant_id, success=True)

        endpoint_tuples = settings.parse_public_endpoints()
        if endpoint_tuples:
            primary_host, primary_port = endpoint_tuples[0]
        else:
            primary_host, primary_port = "127.0.0.1", 5433

        return TenantCreateResponse(
            id=tenant_id,
            admin_user=request.admin_user,
            admin_password=password,
            connection_string=(
                f"postgresql://{keyspace}.{request.admin_user}:{password}"
                f"@{primary_host}:{primary_port}/postgres"
            ),
            created_at=tenant_db.created_at,
        )

    except HTTPException:
        audit.log_tenant_created(tenant_id, success=False, error="HTTP error during creation")
        raise
    except Exception as e:
        audit.log_tenant_created(tenant_id, success=False, error=str(e))
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"Failed to create tenant: {str(e)}",
        )


@router.get(
    "/{tenant_id}",
    response_model=TenantResponseExtended,
    summary="Get tenant details",
)
async def get_tenant(
    tenant_id: str,
    pd: PDClient = Depends(get_pd_client),
    settings: Settings = Depends(get_settings),
    db: Session = Depends(get_db),
):
    tenant = get_tenant_or_404(tenant_id, db)
    ks = pd.get_keyspace(tenant.keyspace)
    if not ks:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=f"Tenant '{tenant_id}' keyspace not found in TiKV",
        )

    endpoint_tuples = settings.parse_public_endpoints()
    endpoints = [
        Endpoint(
            host=host,
            port=port,
            type=EndpointType.LOAD_BALANCER if len(endpoint_tuples) > 1 else EndpointType.PRIMARY,
            priority=100 - i * 10,
            description=f"pg-tikv endpoint {i+1}" if len(endpoint_tuples) > 1 else "pg-tikv primary endpoint",
        )
        for i, (host, port) in enumerate(endpoint_tuples)
    ]

    return TenantResponseExtended(
        id=tenant_id,
        state=ks.get("state", "ENABLED") if isinstance(ks, dict) else "ENABLED",
        endpoints=endpoints,
        is_deleted=tenant.is_deleted,
        created_at=tenant.created_at,
        created_by=tenant.created_by,
        notes=tenant.notes,
        tags=tenant.tags,
        updated_at=tenant.updated_at,
    )


@router.delete(
    "/{tenant_id}",
    response_model=MessageResponse,
    summary="Disable a tenant",
)
async def delete_tenant(
    tenant_id: str,
    pd: PDClient = Depends(get_pd_client),
    db: Session = Depends(get_db),
):
    tenant = get_tenant_or_404(tenant_id, db)

    if not pd.disable_keyspace(tenant.keyspace):
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail="Failed to disable tenant",
        )

    return MessageResponse(message=f"Tenant '{tenant_id}' disabled")


@router.post(
    "/{tenant_id}/connect",
    response_model=TenantConnectResponse,
    summary="Connect to tenant for user management",
)
async def connect_tenant(
    tenant_id: str,
    request: TenantConnectRequest,
    pd: PDClient = Depends(get_pd_client),
    pg: PgTikvClient = Depends(get_pg_client),
    db: Session = Depends(get_db),
):
    tenant = get_tenant_or_404(tenant_id, db)

    if not pd.get_keyspace(tenant.keyspace):
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=f"Tenant '{tenant_id}' keyspace not found",
        )

    if not pg.test_connection(tenant.keyspace, request.admin_user, request.admin_password):
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Invalid tenant credentials",
        )

    session = session_manager.create_session(
        tenant_id=tenant_id,
        keyspace=tenant.keyspace,
        admin_user=request.admin_user,
        admin_password=request.admin_password,
    )

    return TenantConnectResponse(
        session_id=session.session_id,
        expires_at=session.expires_at,
    )


@router.post(
    "/{tenant_id}/observability/bootstrap",
    response_model=MessageResponse,
    summary="Bootstrap per-tenant observability readonly account",
)
async def bootstrap_observability_user(
    tenant_id: str,
    request: TenantConnectRequest,
    pd: PDClient = Depends(get_pd_client),
    pg: PgTikvClient = Depends(get_pg_client),
    db: Session = Depends(get_db),
):
    tenant = get_tenant_or_404(tenant_id, db)

    if not pd.get_keyspace(tenant.keyspace):
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=f"Tenant '{tenant_id}' keyspace not found",
        )

    if not pg.test_connection(tenant.keyspace, request.admin_user, request.admin_password):
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Invalid tenant credentials",
        )

    obs_password = generate_password()
    created = pg.create_user(
        tenant=tenant.keyspace,
        admin_user=request.admin_user,
        admin_password=request.admin_password,
        new_user=OBSERVABILITY_USER,
        new_password=obs_password,
        superuser=False,
    )
    if not created:
        rotated = pg.reset_password(
            tenant=tenant.keyspace,
            admin_user=request.admin_user,
            admin_password=request.admin_password,
            target_user=OBSERVABILITY_USER,
            new_password=obs_password,
        )
        if not rotated:
            raise HTTPException(
                status_code=status.HTTP_502_BAD_GATEWAY,
                detail="Failed to create or rotate observability account",
            )

    tenant.observability_user = OBSERVABILITY_USER
    tenant.observability_password = obs_password
    tenant.updated_at = datetime.now(timezone.utc)
    db.add(tenant)
    db.commit()

    return MessageResponse(
        message=f"Observability account '{OBSERVABILITY_USER}' bootstrapped for tenant '{tenant_id}'"
    )


@router.post(
    "/{tenant_id}/remove",
    response_model=MessageResponse,
    summary="Remove tenant from portal (soft delete)",
)
async def remove_tenant(
    tenant_id: str,
    pd: PDClient = Depends(get_pd_client),
    db: Session = Depends(get_db),
):
    tenant = get_tenant_or_404(tenant_id, db)
    audit = get_audit_service(db)

    try:
        if not pd.disable_keyspace(tenant.keyspace):
            raise HTTPException(
                status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
                detail="Failed to disable keyspace in TiKV",
            )

        tenant.is_deleted = True
        tenant.updated_at = datetime.now(timezone.utc)
        db.commit()

        audit.log_tenant_deleted(tenant_id, success=True)

        return MessageResponse(message=f"Tenant '{tenant_id}' removed from portal")

    except HTTPException:
        audit.log_tenant_deleted(tenant_id, success=False, error="HTTP error during removal")
        raise
    except Exception as e:
        audit.log_tenant_deleted(tenant_id, success=False, error=str(e))
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"Failed to remove tenant: {str(e)}",
        )


@router.put(
    "/{tenant_id}",
    response_model=TenantResponseExtended,
    summary="Update tenant metadata",
)
async def update_tenant(
    tenant_id: str,
    request: TenantUpdate,
    pd: PDClient = Depends(get_pd_client),
    db: Session = Depends(get_db),
):
    tenant = get_tenant_or_404(tenant_id, db)
    audit = get_audit_service(db)

    try:
        if request.notes is not None:
            tenant.notes = request.notes
        if request.tags is not None:
            tenant.tags = request.tags

        tenant.updated_at = datetime.now(timezone.utc)
        db.commit()
        db.refresh(tenant)

        ks = pd.get_keyspace(tenant.keyspace)
        state = ks.get("state", "ENABLED") if isinstance(ks, dict) else "ENABLED"

        audit.log_tenant_updated(
            tenant_id,
            success=True,
            extra_metadata={"notes_updated": request.notes is not None, "tags_updated": request.tags is not None}
        )

        return TenantResponseExtended(
            id=tenant.id,
            state=state,
            is_deleted=tenant.is_deleted,
            created_at=tenant.created_at,
            created_by=tenant.created_by,
            notes=tenant.notes,
            tags=tenant.tags,
            updated_at=tenant.updated_at,
        )

    except Exception as e:
        audit.log_tenant_updated(tenant_id, success=False, error=str(e))
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"Failed to update tenant: {str(e)}",
        )


async def get_tenant_session(
    tenant_id: str,
    x_tenant_session: Optional[str] = Header(None, alias="X-Tenant-Session"),
) -> TenantSession:
    if not x_tenant_session:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Tenant session required. Use POST /api/tenants/{id}/connect first.",
        )

    session = session_manager.validate_session(x_tenant_session, tenant_id)
    if not session:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Invalid or expired session",
        )

    return session


@router.post(
    "/{tenant_id}/query",
    response_model=SqlQueryResponse,
    summary="Execute SQL query",
)
async def execute_query(
    tenant_id: str,
    request: SqlQueryRequest,
    session: TenantSession = Depends(get_tenant_session),
    pg: PgTikvClient = Depends(get_pg_client),
):
    sql = request.sql.strip()
    if not sql:
        return SqlQueryResponse(success=False, error="Empty SQL query")

    stdout, stderr, rc = pg._run_sql(
        session.keyspace,
        session.admin_user,
        session.admin_password,
        sql,
    )

    if rc != 0:
        return SqlQueryResponse(success=False, error=stderr or "Query execution failed")

    return SqlQueryResponse(success=True, result=stdout)


@router.get(
    "/{tenant_id}/observability",
    response_model=TenantObservabilityResponse,
    summary="Get tenant observability metrics (last 1h)",
)
async def get_tenant_observability(
    tenant_id: str,
    db: Session = Depends(get_db),
    pg: PgTikvClient = Depends(get_pg_client),
):
    tenant = get_tenant_or_404(tenant_id, db)

    if not tenant.observability_user or not tenant.observability_password:
        raise HTTPException(
            status_code=status.HTTP_409_CONFLICT,
            detail="Observability account not bootstrapped for this tenant",
        )

    summary, err = pg.get_observability_summary(
        tenant.keyspace,
        tenant.observability_user,
        tenant.observability_password,
    )
    if not summary:
        raise HTTPException(
            status_code=status.HTTP_502_BAD_GATEWAY,
            detail=err or "Failed to fetch observability summary",
        )

    samples, err = pg.get_observability_samples(
        tenant.keyspace,
        tenant.observability_user,
        tenant.observability_password,
    )
    if err:
        samples = []

    return TenantObservabilityResponse(summary=summary, samples=samples)
