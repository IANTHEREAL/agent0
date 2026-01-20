"""Extended tenant models with metadata and audit logging."""

from datetime import datetime
from typing import Any, Dict, List, Optional

from pydantic import BaseModel, Field

from .endpoint import Endpoint


class TenantUpdate(BaseModel):
    """Request body for updating tenant metadata."""

    notes: Optional[str] = Field(
        default=None,
        description="Optional notes about the tenant"
    )
    tags: Optional[List[str]] = Field(
        default=None,
        description="Optional tags for categorization"
    )


class TenantResponseExtended(BaseModel):
    """Extended response with tenant information including metadata."""

    id: str
    state: str
    endpoints: List[Endpoint] = Field(
        default_factory=list,
        description="List of available pg-tikv endpoints for client connections"
    )
    is_deleted: bool = False
    created_at: Optional[datetime] = None
    created_by: Optional[str] = None
    notes: Optional[str] = None
    tags: Optional[List[str]] = None
    updated_at: Optional[datetime] = None

    class Config:
        from_attributes = True


class AuditLogResponse(BaseModel):
    """Response with audit log information."""

    id: int
    timestamp: datetime
    operation_type: str
    resource_type: str
    resource_name: str
    tenant_id: Optional[str] = None
    operator: Optional[str] = None
    success: bool
    error_message: Optional[str] = None
    extra_metadata: Optional[Dict[str, Any]] = None

    class Config:
        from_attributes = True


class AuditLogFilter(BaseModel):
    """Query parameters for filtering audit logs."""

    tenant_id: Optional[str] = Field(
        default=None,
        description="Filter by tenant ID"
    )
    operation_type: Optional[str] = Field(
        default=None,
        description="Filter by operation type"
    )
    resource_type: Optional[str] = Field(
        default=None,
        description="Filter by resource type"
    )
    success: Optional[bool] = Field(
        default=None,
        description="Filter by success status"
    )
    limit: int = Field(
        default=100,
        ge=1,
        le=1000,
        description="Maximum number of logs to return"
    )
    offset: int = Field(
        default=0,
        ge=0,
        description="Number of logs to skip"
    )
