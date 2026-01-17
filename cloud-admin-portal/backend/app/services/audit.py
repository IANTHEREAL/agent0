"""Audit logging service for tracking operations."""

from datetime import datetime, timezone
from enum import Enum
from typing import Any, Dict, Optional

from sqlalchemy.orm import Session

from ..models.db import AuditLogDB


class OperationType(str, Enum):
    """Types of operations that can be audited."""

    CREATE_TENANT = "create_tenant"
    DELETE_TENANT = "delete_tenant"
    UPDATE_TENANT = "update_tenant"
    CONNECT_TENANT = "connect_tenant"

    CREATE_USER = "create_user"
    DELETE_USER = "delete_user"
    RESET_PASSWORD = "reset_password"


class ResourceType(str, Enum):
    """Types of resources that can be audited."""

    TENANT = "tenant"
    USER = "user"


class AuditService:
    """Service for logging audit events."""

    def __init__(self, db: Session):
        """Initialize audit service with database session."""
        self.db = db

    def log(
        self,
        operation_type: OperationType,
        resource_type: ResourceType,
        resource_name: str,
        success: bool,
        tenant_name: Optional[str] = None,
        operator: Optional[str] = None,
        error_message: Optional[str] = None,
        extra_metadata: Optional[Dict[str, Any]] = None,
    ) -> AuditLogDB:
        """Log an audit event.

        Args:
            operation_type: Type of operation performed
            resource_type: Type of resource affected
            resource_name: Name of the resource
            success: Whether the operation succeeded
            tenant_name: Tenant context (for user operations)
            operator: Who performed the operation
            error_message: Error message if operation failed
            extra_metadata: Additional context as dictionary

        Returns:
            The created audit log entry
        """
        log_entry = AuditLogDB(
            timestamp=datetime.now(timezone.utc),
            operation_type=operation_type.value,
            resource_type=resource_type.value,
            resource_name=resource_name,
            tenant_name=tenant_name,
            operator=operator,
            success=success,
            error_message=error_message,
            extra_metadata=extra_metadata,
        )

        self.db.add(log_entry)
        self.db.commit()

        return log_entry

    # Convenience methods for common operations

    def log_tenant_created(
        self,
        tenant_name: str,
        success: bool,
        operator: Optional[str] = None,
        error: Optional[str] = None,
    ) -> AuditLogDB:
        """Log tenant creation."""
        return self.log(
            operation_type=OperationType.CREATE_TENANT,
            resource_type=ResourceType.TENANT,
            resource_name=tenant_name,
            success=success,
            operator=operator,
            error_message=error,
        )

    def log_tenant_deleted(
        self,
        tenant_name: str,
        success: bool,
        operator: Optional[str] = None,
        error: Optional[str] = None,
    ) -> AuditLogDB:
        """Log tenant deletion (soft delete)."""
        return self.log(
            operation_type=OperationType.DELETE_TENANT,
            resource_type=ResourceType.TENANT,
            resource_name=tenant_name,
            success=success,
            operator=operator,
            error_message=error,
        )

    def log_tenant_updated(
        self,
        tenant_name: str,
        success: bool,
        operator: Optional[str] = None,
        error: Optional[str] = None,
        extra_metadata: Optional[Dict[str, Any]] = None,
    ) -> AuditLogDB:
        """Log tenant extra_metadata update."""
        return self.log(
            operation_type=OperationType.UPDATE_TENANT,
            resource_type=ResourceType.TENANT,
            resource_name=tenant_name,
            success=success,
            operator=operator,
            error_message=error,
            extra_metadata=extra_metadata,
        )

    def log_user_created(
        self,
        tenant_name: str,
        username: str,
        success: bool,
        operator: Optional[str] = None,
        error: Optional[str] = None,
    ) -> AuditLogDB:
        """Log user creation."""
        return self.log(
            operation_type=OperationType.CREATE_USER,
            resource_type=ResourceType.USER,
            resource_name=username,
            tenant_name=tenant_name,
            success=success,
            operator=operator,
            error_message=error,
        )

    def log_user_deleted(
        self,
        tenant_name: str,
        username: str,
        success: bool,
        operator: Optional[str] = None,
        error: Optional[str] = None,
    ) -> AuditLogDB:
        """Log user deletion."""
        return self.log(
            operation_type=OperationType.DELETE_USER,
            resource_type=ResourceType.USER,
            resource_name=username,
            tenant_name=tenant_name,
            success=success,
            operator=operator,
            error_message=error,
        )

    def log_password_reset(
        self,
        tenant_name: str,
        username: str,
        success: bool,
        operator: Optional[str] = None,
        error: Optional[str] = None,
    ) -> AuditLogDB:
        """Log password reset."""
        return self.log(
            operation_type=OperationType.RESET_PASSWORD,
            resource_type=ResourceType.USER,
            resource_name=username,
            tenant_name=tenant_name,
            success=success,
            operator=operator,
            error_message=error,
        )


def get_audit_service(db: Session) -> AuditService:
    """Get audit service instance.

    Usage:
        audit = get_audit_service(db)
        audit.log_tenant_created("myapp", success=True)
    """
    return AuditService(db)
