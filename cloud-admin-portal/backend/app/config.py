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

    class Config:
        env_prefix = "PGTIKV_"
        case_sensitive = False

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
