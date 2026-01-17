"""User management API endpoints."""

import secrets
import string
from typing import List, Optional

from fastapi import APIRouter, Depends, Header, HTTPException, status

from ..auth import get_current_user
from ..config import get_settings, Settings
from ..models import (
    UserCreate,
    UserResponse,
    UserCreateResponse,
    PasswordResetResponse,
    MessageResponse,
)
from ..services import PgTikvClient
from ..session import session_manager, TenantSession


router = APIRouter()


def get_pg_client(settings: Settings = Depends(get_settings)) -> PgTikvClient:
    """Get pg-tikv client instance."""
    return PgTikvClient(settings.pg_host, settings.pg_port)


def generate_password(length: int = 16) -> str:
    """Generate a random password."""
    alphabet = string.ascii_letters + string.digits + "!@#$%^&*"
    return "".join(secrets.choice(alphabet) for _ in range(length))


async def get_tenant_session(
    name: str,
    x_tenant_session: Optional[str] = Header(None, alias="X-Tenant-Session"),
) -> TenantSession:
    """Validate tenant session for user management operations.
    
    Args:
        name: Tenant name from path
        x_tenant_session: Session ID from header
    
    Returns:
        Valid TenantSession
    
    Raises:
        HTTPException: If session is missing, invalid, or for wrong tenant
    """
    if not x_tenant_session:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Tenant session required. Use POST /api/tenants/{name}/connect first.",
        )
    
    session = session_manager.validate_session(x_tenant_session, name)
    if not session:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Invalid or expired tenant session",
        )
    
    return session


@router.get(
    "/tenants/{name}/users",
    response_model=List[UserResponse],
    summary="List users in tenant",
    description="List all users in the tenant. Requires tenant session."
)
async def list_users(
    name: str,
    _: str = Depends(get_current_user),
    session: TenantSession = Depends(get_tenant_session),
    pg: PgTikvClient = Depends(get_pg_client),
):
    """List all users in the tenant."""
    users = pg.list_users(name, session.admin_user, session.admin_password)
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
    "/tenants/{name}/users",
    response_model=UserCreateResponse,
    status_code=status.HTTP_201_CREATED,
    summary="Create user in tenant",
    description="Create a new user in the tenant. Requires tenant session."
)
async def create_user(
    name: str,
    request: UserCreate,
    _: str = Depends(get_current_user),
    session: TenantSession = Depends(get_tenant_session),
    pg: PgTikvClient = Depends(get_pg_client),
    settings: Settings = Depends(get_settings),
):
    """Create a new user in the tenant."""
    password = request.password or generate_password()
    
    success = pg.create_user(
        name,
        session.admin_user,
        session.admin_password,
        request.username,
        password,
        request.superuser,
    )
    
    if not success:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail="Failed to create user",
        )
    
    return UserCreateResponse(
        username=request.username,
        password=password,
        connection=f"psql -h {settings.pg_host} -p {settings.pg_port} -U {name}.{request.username}",
    )


@router.delete(
    "/tenants/{name}/users/{username}",
    response_model=MessageResponse,
    summary="Delete user from tenant",
    description="Delete a user from the tenant. Requires tenant session."
)
async def delete_user(
    name: str,
    username: str,
    _: str = Depends(get_current_user),
    session: TenantSession = Depends(get_tenant_session),
    pg: PgTikvClient = Depends(get_pg_client),
):
    """Delete a user from the tenant."""
    success = pg.drop_user(name, session.admin_user, session.admin_password, username)
    
    if not success:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail="Failed to delete user",
        )
    
    return MessageResponse(message=f"User '{username}' deleted")


@router.post(
    "/tenants/{name}/users/{username}/password",
    response_model=PasswordResetResponse,
    summary="Reset user password",
    description="Reset a user's password. Requires tenant session."
)
async def reset_password(
    name: str,
    username: str,
    _: str = Depends(get_current_user),
    session: TenantSession = Depends(get_tenant_session),
    pg: PgTikvClient = Depends(get_pg_client),
):
    """Reset a user's password."""
    new_password = generate_password()
    
    success = pg.reset_password(
        name,
        session.admin_user,
        session.admin_password,
        username,
        new_password,
    )
    
    if not success:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail="Failed to reset password",
        )
    
    return PasswordResetResponse(username=username, password=new_password)
