"""Application configuration using pydantic-settings."""

from functools import lru_cache
from typing import List

from pydantic import AliasChoices, Field
from pydantic_settings import BaseSettings


class Settings(BaseSettings):
    """Application settings loaded from environment variables.
    
    All settings can be overridden via environment variables with PGTIKV_ prefix.
    """
    
    # TiKV PD Configuration
    pd_endpoints: str = Field(
        default="127.0.0.1:2379",
        description="Comma-separated TiKV PD addresses",
        validation_alias=AliasChoices('PD_ENDPOINTS', 'pd_endpoints')  # Read from PD_ENDPOINTS env var
    )
    
    # pg-tikv Server Configuration (Internal)
    pg_host: str = Field(
        default="127.0.0.1",
        description="pg-tikv server host for backend connections"
    )
    pg_port: int = Field(
        default=5433,
        description="pg-tikv server port for backend connections"
    )

    # pg-tikv Public Endpoints (for end users - supports multiple endpoints for load balancing)
    pg_public_endpoints: str = Field(
        default="127.0.0.1:5433",
        description="Comma-separated pg-tikv public endpoints (host:port) for client connections"
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
    
    # Tenant Sessions
    session_ttl_hours: int = Field(
        default=1,
        description="Tenant session validity in hours"
    )

    # Database Configuration
    database_url: str = Field(
        default="sqlite:///backend/data/portal.db",
        description="Database URL for portal data"
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

    # API Key Authentication
    api_keys: str = Field(
        default="",
        description=(
            "Comma-separated API keys for portal authentication. "
            "Empty = auth disabled (dev mode). "
            "Example: PGTIKV_API_KEYS=key1,key2,key3"
        )
    )

    # Reconciler
    reconciler_enabled: bool = Field(
        default=True,
        description="Enable background reconciler that syncs DB state with PD"
    )
    reconciler_interval_seconds: int = Field(
        default=300,
        description="Reconciler check interval in seconds"
    )

    class Config:
        env_prefix = "PGTIKV_"
        case_sensitive = False

    def get_api_key_list(self) -> list[str]:
        """Parse api_keys into a list. Empty list = auth disabled."""
        if not self.api_keys.strip():
            return []
        return [k.strip() for k in self.api_keys.split(",") if k.strip()]

    def parse_public_endpoints(self) -> list[tuple[str, int]]:
        """Parse pg_public_endpoints into list of (host, port) tuples.

        Format: "host1:port1,host2:port2,..."
        Example: "pg1.example.com:5433,pg2.example.com:5433"

        Returns:
            List of (host, port) tuples
        """
        endpoints = []
        for endpoint in self.pg_public_endpoints.split(","):
            endpoint = endpoint.strip()
            if not endpoint:
                continue

            if ":" in endpoint:
                host, port_str = endpoint.rsplit(":", 1)
                try:
                    port = int(port_str)
                    endpoints.append((host.strip(), port))
                except ValueError:
                    # Invalid port, skip this endpoint
                    continue
            else:
                # No port specified, use default
                endpoints.append((endpoint, 5433))

        return endpoints


@lru_cache()
def get_settings() -> Settings:
    """Get cached settings instance."""
    return Settings()
