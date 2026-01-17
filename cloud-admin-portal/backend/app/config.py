"""Application configuration using pydantic-settings."""

import secrets
from functools import lru_cache
from typing import List

from pydantic import Field
from pydantic_settings import BaseSettings


class Settings(BaseSettings):
    """Application settings loaded from environment variables.
    
    All settings can be overridden via environment variables with PGTIKV_ prefix.
    Example: PGTIKV_ADMIN_PASSWORD=mysecret
    """
    
    # TiKV PD Configuration
    pd_endpoints: str = Field(
        default="127.0.0.1:2379",
        description="Comma-separated TiKV PD addresses"
    )
    
    # pg-tikv Server Configuration
    pg_host: str = Field(
        default="127.0.0.1",
        description="pg-tikv server host"
    )
    pg_port: int = Field(
        default=5433,
        description="pg-tikv server port"
    )
    
    # API Server Configuration
    api_port: int = Field(
        default=8080,
        description="API server port"
    )
    api_host: str = Field(
        default="0.0.0.0",
        description="API server bind address"
    )
    
    # Authentication
    admin_password: str = Field(
        default="admin",
        description="Admin login password. MUST change in production!"
    )
    jwt_secret: str = Field(
        default_factory=lambda: secrets.token_hex(32),
        description="JWT signing secret. Auto-generated if not set."
    )
    jwt_expiry_hours: int = Field(
        default=24,
        description="JWT token validity in hours"
    )
    jwt_algorithm: str = Field(
        default="HS256",
        description="JWT signing algorithm"
    )
    
    # Tenant Sessions
    session_ttl_hours: int = Field(
        default=1,
        description="Tenant session validity in hours"
    )
    
    # CORS Configuration
    cors_origins: List[str] = Field(
        default=["http://localhost:5173", "http://localhost:3000"],
        description="Allowed CORS origins"
    )
    
    # Debug
    debug: bool = Field(
        default=False,
        description="Enable debug mode"
    )

    class Config:
        env_prefix = "PGTIKV_"
        case_sensitive = False


@lru_cache()
def get_settings() -> Settings:
    """Get cached settings instance."""
    return Settings()
