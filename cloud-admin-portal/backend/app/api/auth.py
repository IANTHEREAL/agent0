"""Authentication API endpoints."""

from fastapi import APIRouter, Depends, HTTPException, status

from ..auth import create_access_token, get_current_user
from ..config import get_settings
from ..models import LoginRequest, LoginResponse, UserInfoResponse


router = APIRouter()


@router.post(
    "/login",
    response_model=LoginResponse,
    summary="Login to get JWT token",
    description="""
    Authenticate with admin password and receive a JWT token.
    
    The token should be included in subsequent requests as:
    `Authorization: Bearer <token>`
    """
)
async def login(request: LoginRequest):
    """Authenticate and return JWT token."""
    settings = get_settings()
    
    if request.password != settings.admin_password:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Invalid password",
        )
    
    token, expires_at = create_access_token(subject="admin")
    return LoginResponse(token=token, expires_at=expires_at)


@router.get(
    "/me",
    response_model=UserInfoResponse,
    summary="Get current user info",
    description="Returns information about the currently authenticated user."
)
async def get_me(user: str = Depends(get_current_user)):
    """Get current authenticated user info."""
    return UserInfoResponse(user=user)
