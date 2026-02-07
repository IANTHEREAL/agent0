import json
from datetime import datetime, timezone

from sqlalchemy import Boolean, Column, DateTime, Integer, String, Text, UniqueConstraint
from sqlalchemy.orm import declarative_base
from sqlalchemy.types import TypeDecorator

Base = declarative_base()

TENANT_STATES = ("CREATING", "CREATE_FAILED", "ACTIVE", "SUSPENDED", "DISABLING", "DISABLED")


class JSONType(TypeDecorator):
    impl = Text
    cache_ok = True

    def process_bind_param(self, value, dialect):
        if value is not None:
            return json.dumps(value)
        return value

    def process_result_value(self, value, dialect):
        if value is not None:
            return json.loads(value)
        return value


class TenantDB(Base):
    __tablename__ = "tenants"

    _pk = Column("pk", Integer, primary_key=True, autoincrement=True)
    id = Column(String(14), unique=True, nullable=False, index=True)
    keyspace = Column(String(64), unique=True, nullable=False, index=True)
    state = Column(String(20), nullable=False, default="ACTIVE", index=True)
    state_reason = Column(Text, nullable=True)
    created_at = Column(DateTime, default=lambda: datetime.now(timezone.utc), nullable=False)
    created_by = Column(String(255), nullable=True)
    notes = Column(Text, nullable=True)
    tags = Column(JSONType, nullable=True)
    updated_at = Column(DateTime, nullable=True)

    @property
    def is_deleted(self) -> bool:
        return self.state in ("DISABLED", "CREATE_FAILED")


class TenantCredentialDB(Base):
    __tablename__ = "tenant_credentials"

    id = Column(Integer, primary_key=True, autoincrement=True)
    tenant_id = Column(String(14), nullable=False, index=True)
    credential_type = Column(String(20), nullable=False)
    username = Column(String(255), nullable=False)
    password_enc = Column(Text, nullable=False)
    key_version = Column(Integer, nullable=False, default=1)
    created_at = Column(DateTime, default=lambda: datetime.now(timezone.utc), nullable=False)
    rotated_at = Column(DateTime, nullable=True)

    __table_args__ = (
        UniqueConstraint("tenant_id", "credential_type", "username", name="uq_tenant_cred"),
    )


class AuditLogDB(Base):
    __tablename__ = "audit_logs"

    id = Column(Integer, primary_key=True, autoincrement=True)
    timestamp = Column(DateTime, default=lambda: datetime.now(timezone.utc), nullable=False, index=True)
    operation_type = Column(String(50), nullable=False, index=True)
    resource_type = Column(String(50), nullable=False)
    resource_name = Column(String(255), nullable=False, index=True)
    tenant_id = Column(String(14), nullable=True, index=True)
    operator = Column(String(255), nullable=True)
    success = Column(Boolean, nullable=False, index=True)
    error_message = Column(Text, nullable=True)
    extra_metadata = Column(JSONType, nullable=True)
