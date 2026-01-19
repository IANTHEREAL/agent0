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
)
async def query_audit_logs(
    tenant_id: Optional[str] = Query(None, description="Filter by tenant ID"),
    operation_type: Optional[str] = Query(None, description="Filter by operation type"),
    resource_type: Optional[str] = Query(None, description="Filter by resource type"),
    success: Optional[bool] = Query(None, description="Filter by success status"),
    limit: int = Query(100, ge=1, le=1000, description="Maximum number of logs to return"),
    offset: int = Query(0, ge=0, description="Number of logs to skip"),
    db: Session = Depends(get_db),
):
    query = db.query(AuditLogDB)

    if tenant_id is not None:
        query = query.filter(AuditLogDB.tenant_id == tenant_id)

    if operation_type is not None:
        query = query.filter(AuditLogDB.operation_type == operation_type)

    if resource_type is not None:
        query = query.filter(AuditLogDB.resource_type == resource_type)

    if success is not None:
        query = query.filter(AuditLogDB.success == success)

    query = query.order_by(AuditLogDB.timestamp.desc())

    logs = query.offset(offset).limit(limit).all()

    return [
        AuditLogResponse(
            id=log.id,
            timestamp=log.timestamp,
            operation_type=log.operation_type,
            resource_type=log.resource_type,
            resource_name=log.resource_name,
            tenant_id=log.tenant_id,
            operator=log.operator,
            success=log.success,
            error_message=log.error_message,
            extra_metadata=log.extra_metadata,
        )
        for log in logs
    ]
