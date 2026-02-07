import secrets
import string
from typing import Optional

from fastapi import Depends, Header, HTTPException, status
from sqlalchemy.orm import Session

from ..config import get_settings, Settings
from ..database import get_db
from ..models.db import TenantDB
from ..services import PDClient, PgTikvClient
from ..session import session_manager, TenantSession


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
        TenantDB.state.notin_(("DISABLED", "CREATE_FAILED")),
    ).first()
    if not tenant:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=f"Tenant '{tenant_id}' not found",
        )
    return tenant


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
