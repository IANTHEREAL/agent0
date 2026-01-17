"""Pydantic models for request/response validation."""

from .auth import LoginRequest, LoginResponse, UserInfoResponse
from .tenant import (
    TenantCreate,
    TenantResponse,
    TenantCreateResponse,
    TenantConnectRequest,
    TenantConnectResponse,
)
from .user import (
    UserCreate,
    UserResponse,
    UserCreateResponse,
    PasswordResetResponse,
)
from .common import MessageResponse, HealthResponse, ErrorResponse

__all__ = [
    # Auth
    "LoginRequest",
    "LoginResponse",
    "UserInfoResponse",
    # Tenant
    "TenantCreate",
    "TenantResponse",
    "TenantCreateResponse",
    "TenantConnectRequest",
    "TenantConnectResponse",
    # User
    "UserCreate",
    "UserResponse",
    "UserCreateResponse",
    "PasswordResetResponse",
    # Common
    "MessageResponse",
    "HealthResponse",
    "ErrorResponse",
]
