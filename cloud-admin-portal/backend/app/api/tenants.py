from datetime import datetime, timezone
from typing import Optional

from fastapi import APIRouter, Depends, HTTPException, Query, status
from sqlalchemy.orm import Session

from ..auth import require_api_key
from ..config import get_settings, Settings
from ..database import get_db
from ..models import (
    Endpoint,
    EndpointType,
    TenantCreate,
    TenantCreateResponse,
    TenantConnectRequest,
    TenantConnectResponse,
    TenantUpdate,
    TenantResponseExtended,
    TenantListResponse,
    MessageResponse,
    SqlQueryRequest,
    SqlQueryResponse,
    TenantObservabilityResponse,
)
from ..models.db import TenantDB, TenantCredentialDB
from ..services import PDClient, PgTikvClient, generate_tenant_id, make_keyspace
from ..services.audit import get_audit_service
from ..session import session_manager, TenantSession
from .deps import (
    get_pd_client,
    get_pg_client,
    get_tenant_or_404,
    get_tenant_session,
    generate_password,
)

router = APIRouter()

OBSERVABILITY_USER = "_pgtikv_sys_observer"


def _tenant_to_response(tenant: TenantDB, state_override: Optional[str] = None) -> TenantResponseExtended:
    return TenantResponseExtended(
        id=tenant.id,
        state=state_override or tenant.state,
        is_deleted=tenant.is_deleted,
        created_at=tenant.created_at,
        created_by=tenant.created_by,
        notes=tenant.notes,
        tags=tenant.tags,
        updated_at=tenant.updated_at,
        state_reason=tenant.state_reason,
    )


@router.get("", response_model=TenantListResponse)
async def list_tenants(
    page: int = Query(1, ge=1),
    size: int = Query(50, ge=1, le=200),
    state: Optional[str] = Query(None),
    q: Optional[str] = Query(None),
    db: Session = Depends(get_db),
    _auth=Depends(require_api_key),
):
    query = db.query(TenantDB).filter(
        TenantDB.state.notin_(("CREATE_FAILED",))
    )

    if state:
        query = query.filter(TenantDB.state == state)
    if q:
        query = query.filter(TenantDB.id.contains(q))

    total = query.count()
    tenants = (
        query.order_by(TenantDB.created_at.desc())
        .offset((page - 1) * size)
        .limit(size)
        .all()
    )

    return TenantListResponse(
        items=[_tenant_to_response(t) for t in tenants],
        total=total,
        page=page,
        size=size,
    )


@router.post("", response_model=TenantCreateResponse, status_code=status.HTTP_201_CREATED)
async def create_tenant(
    request: TenantCreate,
    pd: PDClient = Depends(get_pd_client),
    pg: PgTikvClient = Depends(get_pg_client),
    settings: Settings = Depends(get_settings),
    db: Session = Depends(get_db),
    _auth=Depends(require_api_key),
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

    tenant_db = TenantDB(
        id=tenant_id,
        keyspace=keyspace,
        state="CREATING",
        created_at=datetime.now(timezone.utc),
    )
    db.add(tenant_db)
    db.flush()

    try:
        if not pd.create_keyspace(keyspace):
            tenant_db.state = "CREATE_FAILED"
            tenant_db.state_reason = "Failed to create keyspace in PD"
            db.commit()
            raise HTTPException(
                status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
                detail="Failed to create keyspace in TiKV",
            )

        password = request.admin_password or generate_password()

        if not await pg.bootstrap_admin_password(keyspace, request.admin_user, password):
            tenant_db.state = "CREATE_FAILED"
            tenant_db.state_reason = "Keyspace created but password bootstrap failed"
            db.commit()
            raise HTTPException(
                status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
                detail="Failed to set admin password. Keyspace created but password unchanged.",
            )

        tenant_db.state = "ACTIVE"
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
        tenant_db.state = "CREATE_FAILED"
        tenant_db.state_reason = str(e)
        db.commit()
        audit.log_tenant_created(tenant_id, success=False, error=str(e))
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"Failed to create tenant: {str(e)}",
        )


@router.get("/{tenant_id}", response_model=TenantResponseExtended)
async def get_tenant(
    tenant_id: str,
    pd: PDClient = Depends(get_pd_client),
    settings: Settings = Depends(get_settings),
    db: Session = Depends(get_db),
    _auth=Depends(require_api_key),
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

    resp = _tenant_to_response(tenant)
    resp.endpoints = endpoints
    return resp


@router.delete("/{tenant_id}", response_model=MessageResponse)
async def delete_tenant(
    tenant_id: str,
    pd: PDClient = Depends(get_pd_client),
    db: Session = Depends(get_db),
    _auth=Depends(require_api_key),
):
    tenant = get_tenant_or_404(tenant_id, db)
    audit = get_audit_service(db)

    try:
        tenant.state = "DISABLING"
        tenant.updated_at = datetime.now(timezone.utc)
        db.flush()

        if not pd.disable_keyspace(tenant.keyspace):
            tenant.state = "ACTIVE"
            tenant.state_reason = "Failed to disable keyspace in PD"
            db.flush()
            raise HTTPException(
                status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
                detail="Failed to disable tenant",
            )

        tenant.state = "DISABLED"
        tenant.state_reason = "Deleted via API"
        db.flush()

        audit.log_tenant_deleted(tenant_id, success=True)
        return MessageResponse(message=f"Tenant '{tenant_id}' disabled")

    except HTTPException:
        audit.log_tenant_deleted(tenant_id, success=False, error="HTTP error during deletion")
        raise
    except Exception as e:
        audit.log_tenant_deleted(tenant_id, success=False, error=str(e))
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"Failed to delete tenant: {str(e)}",
        )


@router.post("/{tenant_id}/connect", response_model=TenantConnectResponse)
async def connect_tenant(
    tenant_id: str,
    request: TenantConnectRequest,
    pd: PDClient = Depends(get_pd_client),
    pg: PgTikvClient = Depends(get_pg_client),
    db: Session = Depends(get_db),
    _auth=Depends(require_api_key),
):
    tenant = get_tenant_or_404(tenant_id, db)

    if tenant.state == "SUSPENDED":
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN,
            detail="Tenant is suspended",
        )

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


@router.post("/{tenant_id}/observability/bootstrap", response_model=MessageResponse)
async def bootstrap_observability_user(
    tenant_id: str,
    request: TenantConnectRequest,
    pd: PDClient = Depends(get_pd_client),
    pg: PgTikvClient = Depends(get_pg_client),
    db: Session = Depends(get_db),
    _auth=Depends(require_api_key),
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

    existing_cred = db.query(TenantCredentialDB).filter(
        TenantCredentialDB.tenant_id == tenant_id,
        TenantCredentialDB.credential_type == "OBSERVABILITY",
        TenantCredentialDB.username == OBSERVABILITY_USER,
    ).first()

    if existing_cred:
        existing_cred.password_enc = obs_password
        existing_cred.rotated_at = datetime.now(timezone.utc)
    else:
        db.add(TenantCredentialDB(
            tenant_id=tenant_id,
            credential_type="OBSERVABILITY",
            username=OBSERVABILITY_USER,
            password_enc=obs_password,
        ))

    tenant.updated_at = datetime.now(timezone.utc)
    db.flush()

    return MessageResponse(
        message=f"Observability account '{OBSERVABILITY_USER}' bootstrapped for tenant '{tenant_id}'"
    )


@router.post("/{tenant_id}/remove", response_model=MessageResponse)
async def remove_tenant(
    tenant_id: str,
    pd: PDClient = Depends(get_pd_client),
    db: Session = Depends(get_db),
    _auth=Depends(require_api_key),
):
    tenant = get_tenant_or_404(tenant_id, db)
    audit = get_audit_service(db)

    try:
        tenant.state = "DISABLING"
        tenant.updated_at = datetime.now(timezone.utc)
        db.flush()

        if not pd.disable_keyspace(tenant.keyspace):
            tenant.state = "ACTIVE"
            tenant.state_reason = "Failed to disable keyspace in PD"
            db.flush()
            raise HTTPException(
                status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
                detail="Failed to disable keyspace in TiKV",
            )

        tenant.state = "DISABLED"
        tenant.state_reason = "Removed via portal"
        db.flush()

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


@router.put("/{tenant_id}", response_model=TenantResponseExtended)
async def update_tenant(
    tenant_id: str,
    request: TenantUpdate,
    db: Session = Depends(get_db),
    _auth=Depends(require_api_key),
):
    tenant = get_tenant_or_404(tenant_id, db)
    audit = get_audit_service(db)

    try:
        if request.notes is not None:
            tenant.notes = request.notes
        if request.tags is not None:
            tenant.tags = request.tags

        tenant.updated_at = datetime.now(timezone.utc)
        db.flush()

        audit.log_tenant_updated(
            tenant_id,
            success=True,
            extra_metadata={"notes_updated": request.notes is not None, "tags_updated": request.tags is not None}
        )

        return _tenant_to_response(tenant)

    except Exception as e:
        audit.log_tenant_updated(tenant_id, success=False, error=str(e))
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"Failed to update tenant: {str(e)}",
        )


@router.post("/{tenant_id}/query", response_model=SqlQueryResponse)
async def execute_query(
    tenant_id: str,
    request: SqlQueryRequest,
    session: TenantSession = Depends(get_tenant_session),
    pg: PgTikvClient = Depends(get_pg_client),
    _auth=Depends(require_api_key),
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


@router.get("/{tenant_id}/observability", response_model=TenantObservabilityResponse)
async def get_tenant_observability(
    tenant_id: str,
    db: Session = Depends(get_db),
    pg: PgTikvClient = Depends(get_pg_client),
    _auth=Depends(require_api_key),
):
    tenant = get_tenant_or_404(tenant_id, db)

    cred = db.query(TenantCredentialDB).filter(
        TenantCredentialDB.tenant_id == tenant_id,
        TenantCredentialDB.credential_type == "OBSERVABILITY",
    ).first()

    if not cred:
        raise HTTPException(
            status_code=status.HTTP_409_CONFLICT,
            detail="Observability account not bootstrapped for this tenant",
        )

    summary, err = pg.get_observability_summary(
        tenant.keyspace,
        cred.username,
        cred.password_enc,
    )
    if not summary:
        raise HTTPException(
            status_code=status.HTTP_502_BAD_GATEWAY,
            detail=err or "Failed to fetch observability summary",
        )

    samples, err = pg.get_observability_samples(
        tenant.keyspace,
        cred.username,
        cred.password_enc,
    )
    if err:
        samples = []

    return TenantObservabilityResponse(summary=summary, samples=samples)
