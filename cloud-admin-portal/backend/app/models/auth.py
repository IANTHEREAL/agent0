"""Authentication models."""

from datetime import datetime
from pydantic import BaseModel


class LoginRequest(BaseModel):
    """Request body for login."""
    
    password: str


class LoginResponse(BaseModel):
    """Response after successful login."""
    
    token: str
    expires_at: datetime


class UserInfoResponse(BaseModel):
    """Response for /auth/me endpoint."""
    
    user: str
