"""User-related models."""

from typing import Optional

from pydantic import BaseModel, Field


class UserCreate(BaseModel):
    """Request body for creating a user."""
    
    username: str = Field(
        ...,
        min_length=1,
        max_length=64,
        description="Username to create"
    )
    password: Optional[str] = Field(
        default=None,
        description="Password (auto-generated if not provided)"
    )
    superuser: bool = Field(
        default=False,
        description="Create as superuser"
    )


class UserResponse(BaseModel):
    """Response with user information."""
    
    name: str
    is_superuser: bool
    can_login: bool
    can_create_db: bool
    can_create_role: bool


class UserCreateResponse(BaseModel):
    """Response after creating a user."""
    
    username: str
    password: str
    connection: str


class PasswordResetResponse(BaseModel):
    """Response after resetting a password."""
    
    username: str
    password: str
