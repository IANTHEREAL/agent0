"""Audit log API endpoints."""

from typing import List, Optional

from fastapi import APIRouter, Depends, Query
from sqlalchemy.orm import Session

from ..database import get_db
from ..models import AuditLogResponse
from ..models.db import AuditLogDB


router = APIRouter()


@router.get(
    "",
    response_model=List[AuditLogResponse],
    summary="Query audit logs",
    description="""
    Query audit logs with optional filters.

    Logs are returned in reverse chronological order (newest first).
    """
)
async def query_audit_logs(
    tenant_name: Optional[str] = Query(None, description="Filter by tenant name"),
    operation_type: Optional[str] = Query(None, description="Filter by operation type"),
    resource_type: Optional[str] = Query(None, description="Filter by resource type"),
    success: Optional[bool] = Query(None, description="Filter by success status"),
    limit: int = Query(100, ge=1, le=1000, description="Maximum number of logs to return"),
    offset: int = Query(0, ge=0, description="Number of logs to skip"),
    db: Session = Depends(get_db),
):
    """Query audit logs with filters."""
    query = db.query(AuditLogDB)

    # Apply filters
    if tenant_name is not None:
        query = query.filter(AuditLogDB.tenant_name == tenant_name)

    if operation_type is not None:
        query = query.filter(AuditLogDB.operation_type == operation_type)

    if resource_type is not None:
        query = query.filter(AuditLogDB.resource_type == resource_type)

    if success is not None:
        query = query.filter(AuditLogDB.success == success)

    # Order by newest first
    query = query.order_by(AuditLogDB.timestamp.desc())

    # Apply pagination
    logs = query.offset(offset).limit(limit).all()

    return [
        AuditLogResponse(
            id=log.id,
            timestamp=log.timestamp,
            operation_type=log.operation_type,
            resource_type=log.resource_type,
            resource_name=log.resource_name,
            tenant_name=log.tenant_name,
            operator=log.operator,
            success=log.success,
            error_message=log.error_message,
            metadata=log.metadata,
        )
        for log in logs
    ]
