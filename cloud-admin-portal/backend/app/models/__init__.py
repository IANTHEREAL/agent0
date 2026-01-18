"""Pydantic models for request/response validation."""

from .auth import LoginRequest, LoginResponse, UserInfoResponse
from .endpoint import Endpoint, EndpointType
from .tenant import (
    TenantCreate,
    TenantResponse,
    TenantCreateResponse,
    TenantConnectRequest,
    TenantConnectResponse,
)
from .tenant_extended import (
    TenantUpdate,
    TenantResponseExtended,
    AuditLogResponse,
    AuditLogFilter,
)
from .user import (
    UserCreate,
    UserResponse,
    UserCreateResponse,
    PasswordResetResponse,
)
from .common import MessageResponse, HealthResponse, ErrorResponse, SqlQueryRequest, SqlQueryResponse

__all__ = [
    # Auth
    "LoginRequest",
    "LoginResponse",
    "UserInfoResponse",
    # Endpoint
    "Endpoint",
    "EndpointType",
    # Tenant
    "TenantCreate",
    "TenantResponse",
    "TenantCreateResponse",
    "TenantConnectRequest",
    "TenantConnectResponse",
    # Tenant Extended
    "TenantUpdate",
    "TenantResponseExtended",
    "AuditLogResponse",
    "AuditLogFilter",
    # User
    "UserCreate",
    "UserResponse",
    "UserCreateResponse",
    "PasswordResetResponse",
    # Common
    "MessageResponse",
    "HealthResponse",
    "ErrorResponse",
    # SQL
    "SqlQueryRequest",
    "SqlQueryResponse",
]
