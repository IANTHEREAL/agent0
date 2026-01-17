"""SQLAlchemy ORM models for portal database."""

import json
from datetime import datetime, timezone
from typing import List, Optional

from sqlalchemy import Boolean, Column, DateTime, Integer, String, Text
from sqlalchemy.ext.declarative import declarative_base
from sqlalchemy.types import TypeDecorator

Base = declarative_base()


class JSONType(TypeDecorator):
    """JSON type for SQLite (stores as TEXT)."""

    impl = Text
    cache_ok = True

    def process_bind_param(self, value, dialect):
        """Convert Python value to database value."""
        if value is not None:
            return json.dumps(value)
        return value

    def process_result_value(self, value, dialect):
        """Convert database value to Python value."""
        if value is not None:
            return json.loads(value)
        return value


class TenantDB(Base):
    """Tenant metadata table."""

    __tablename__ = "tenants"

    id = Column(Integer, primary_key=True, autoincrement=True)
    name = Column(String(255), unique=True, nullable=False, index=True)
    is_deleted = Column(Boolean, default=False, nullable=False, index=True)
    created_at = Column(DateTime, default=lambda: datetime.now(timezone.utc), nullable=False)
    created_by = Column(String(255), nullable=True)
    notes = Column(Text, nullable=True)
    tags = Column(JSONType, nullable=True)  # List[str] stored as JSON
    updated_at = Column(DateTime, nullable=True)

    def __repr__(self):
        return f"<TenantDB(name='{self.name}', is_deleted={self.is_deleted})>"


class AuditLogDB(Base):
    """Audit log table for tracking operations."""

    __tablename__ = "audit_logs"

    id = Column(Integer, primary_key=True, autoincrement=True)
    timestamp = Column(DateTime, default=lambda: datetime.now(timezone.utc), nullable=False, index=True)
    operation_type = Column(String(50), nullable=False, index=True)  # create_tenant, delete_tenant, etc.
    resource_type = Column(String(50), nullable=False)  # tenant or user
    resource_name = Column(String(255), nullable=False, index=True)
    tenant_name = Column(String(255), nullable=True, index=True)  # Context for user operations
    operator = Column(String(255), nullable=True)  # Who performed the operation
    success = Column(Boolean, nullable=False, index=True)
    error_message = Column(Text, nullable=True)
    extra_metadata = Column(JSONType, nullable=True)  # Additional context as JSON

    def __repr__(self):
        return f"<AuditLogDB(operation='{self.operation_type}', resource='{self.resource_name}', success={self.success})>"
