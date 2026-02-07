from datetime import datetime
from typing import Any, Dict, List, Optional

from pydantic import BaseModel, Field

from .endpoint import Endpoint


class TenantUpdate(BaseModel):
    notes: Optional[str] = None
    tags: Optional[List[str]] = None


class TenantResponseExtended(BaseModel):
    id: str
    state: str
    endpoints: List[Endpoint] = Field(default_factory=list)
    is_deleted: bool = False
    created_at: Optional[datetime] = None
    created_by: Optional[str] = None
    notes: Optional[str] = None
    tags: Optional[List[str]] = None
    updated_at: Optional[datetime] = None
    state_reason: Optional[str] = None

    class Config:
        from_attributes = True


class TenantListResponse(BaseModel):
    items: List[TenantResponseExtended]
    total: int
    page: int
    size: int


class AuditLogResponse(BaseModel):
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
    tenant_id: Optional[str] = None
    operation_type: Optional[str] = None
    resource_type: Optional[str] = None
    success: Optional[bool] = None
    limit: int = Field(default=100, ge=1, le=1000)
    offset: int = Field(default=0, ge=0)
