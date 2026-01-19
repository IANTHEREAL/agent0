"""Tenant-related models."""

from datetime import datetime
from typing import Optional

from pydantic import BaseModel, Field

from .endpoint import Endpoint


class TenantCreate(BaseModel):
    """Request body for creating a tenant."""
    
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

    id: str = Field(description="12-char tenant identifier")
    state: str
    endpoints: list[Endpoint] = Field(
        default_factory=list,
        description="List of available pg-tikv endpoints for client connections"
    )


class TenantCreateResponse(BaseModel):
    """Response after creating a tenant."""
    
    id: str = Field(description="12-char tenant identifier")
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
