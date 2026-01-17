"""Tenant-related models."""

from datetime import datetime
from typing import Optional

from pydantic import BaseModel, Field


class TenantCreate(BaseModel):
    """Request body for creating a tenant."""
    
    name: str = Field(
        ...,
        min_length=3,
        max_length=64,
        pattern=r'^[a-z0-9_]+$',
        description="Tenant name (lowercase alphanumeric with underscores)"
    )
    admin_user: str = Field(
        default="admin",
        description="Admin username"
    )
    admin_password: Optional[str] = Field(
        default=None,
        description="Admin password (auto-generated if not provided)"
    )


class TenantResponse(BaseModel):
    """Response with tenant information."""
    
    name: str
    state: str
    host: Optional[str] = None
    port: Optional[int] = None


class TenantCreateResponse(BaseModel):
    """Response after creating a tenant."""
    
    name: str
    admin_user: str
    admin_password: str
    connection_string: str
    created_at: datetime


class TenantConnectRequest(BaseModel):
    """Request body for connecting to a tenant."""
    
    admin_user: str = Field(
        default="admin",
        description="Admin username"
    )
    admin_password: str = Field(
        ...,
        description="Admin password"
    )


class TenantConnectResponse(BaseModel):
    """Response after connecting to a tenant."""
    
    session_id: str
    expires_at: datetime
