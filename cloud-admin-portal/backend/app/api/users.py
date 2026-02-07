from typing import List

from fastapi import APIRouter, Depends, HTTPException, status
from sqlalchemy.orm import Session

from ..auth import require_api_key
from ..config import get_settings, Settings
from ..database import get_db
from ..models import (
    UserCreate,
    UserResponse,
    UserCreateResponse,
    PasswordResetResponse,
    MessageResponse,
)
from ..services import PgTikvClient
from ..services.audit import get_audit_service
from ..session import TenantSession
from .deps import get_pg_client, get_tenant_or_404, get_tenant_session, generate_password

router = APIRouter()


@router.get("/tenants/{tenant_id}/users", response_model=List[UserResponse])
async def list_users(
    tenant_id: str,
    session: TenantSession = Depends(get_tenant_session),
    pg: PgTikvClient = Depends(get_pg_client),
    db: Session = Depends(get_db),
    _auth=Depends(require_api_key),
):
    tenant = get_tenant_or_404(tenant_id, db)
    users = pg.list_users(tenant.keyspace, session.admin_user, session.admin_password)
    return [
        UserResponse(
            name=u.name,
            is_superuser=u.is_superuser,
            can_login=u.can_login,
            can_create_db=u.can_create_db,
            can_create_role=u.can_create_role,
        )
        for u in users
    ]


@router.post(
    "/tenants/{tenant_id}/users",
    response_model=UserCreateResponse,
    status_code=status.HTTP_201_CREATED,
)
async def create_user(
    tenant_id: str,
    request: UserCreate,
    session: TenantSession = Depends(get_tenant_session),
    pg: PgTikvClient = Depends(get_pg_client),
    settings: Settings = Depends(get_settings),
    db: Session = Depends(get_db),
    _auth=Depends(require_api_key),
):
    tenant = get_tenant_or_404(tenant_id, db)
    password = request.password or generate_password()
    audit = get_audit_service(db)

    try:
        success = pg.create_user(
            tenant.keyspace,
            session.admin_user,
            session.admin_password,
            request.username,
            password,
            request.superuser,
        )

        if not success:
            audit.log_user_created(tenant_id, request.username, success=False, error="PG client returned failure")
            raise HTTPException(
                status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
                detail="Failed to create user",
            )

        audit.log_user_created(tenant_id, request.username, success=True, operator=session.admin_user)

        return UserCreateResponse(
            username=request.username,
            password=password,
            connection=f"psql -h {settings.pg_host} -p {settings.pg_port} -U {tenant.keyspace}.{request.username}",
        )

    except HTTPException:
        raise
    except Exception as e:
        audit.log_user_created(tenant_id, request.username, success=False, error=str(e))
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"Failed to create user: {str(e)}",
        )


@router.delete("/tenants/{tenant_id}/users/{username}", response_model=MessageResponse)
async def delete_user(
    tenant_id: str,
    username: str,
    session: TenantSession = Depends(get_tenant_session),
    pg: PgTikvClient = Depends(get_pg_client),
    db: Session = Depends(get_db),
    _auth=Depends(require_api_key),
):
    tenant = get_tenant_or_404(tenant_id, db)
    audit = get_audit_service(db)

    try:
        success = pg.drop_user(tenant.keyspace, session.admin_user, session.admin_password, username)

        if not success:
            audit.log_user_deleted(tenant_id, username, success=False, error="PG client returned failure")
            raise HTTPException(
                status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
                detail="Failed to delete user",
            )

        audit.log_user_deleted(tenant_id, username, success=True, operator=session.admin_user)
        return MessageResponse(message=f"User '{username}' deleted")

    except HTTPException:
        raise
    except Exception as e:
        audit.log_user_deleted(tenant_id, username, success=False, error=str(e))
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"Failed to delete user: {str(e)}",
        )


@router.post("/tenants/{tenant_id}/users/{username}/password", response_model=PasswordResetResponse)
async def reset_password(
    tenant_id: str,
    username: str,
    session: TenantSession = Depends(get_tenant_session),
    pg: PgTikvClient = Depends(get_pg_client),
    db: Session = Depends(get_db),
    _auth=Depends(require_api_key),
):
    tenant = get_tenant_or_404(tenant_id, db)
    new_password = generate_password()
    audit = get_audit_service(db)

    try:
        success = pg.reset_password(
            tenant.keyspace,
            session.admin_user,
            session.admin_password,
            username,
            new_password,
        )

        if not success:
            audit.log_password_reset(tenant_id, username, success=False, error="PG client returned failure")
            raise HTTPException(
                status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
                detail="Failed to reset password",
            )

        audit.log_password_reset(tenant_id, username, success=True, operator=session.admin_user)
        return PasswordResetResponse(username=username, password=new_password)

    except HTTPException:
        raise
    except Exception as e:
        audit.log_password_reset(tenant_id, username, success=False, error=str(e))
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"Failed to reset password: {str(e)}",
        )
