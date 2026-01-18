"""Endpoint models for pg-tikv connections."""

from enum import Enum
from typing import Optional

from pydantic import BaseModel, Field


class EndpointType(str, Enum):
    """Type of database endpoint."""

    PRIMARY = "primary"  # Primary/master node for reads and writes
    REPLICA = "replica"  # Read-only replica
    LOAD_BALANCER = "load_balancer"  # Load balancer endpoint


class Endpoint(BaseModel):
    """Database connection endpoint with metadata for routing and load balancing."""

    host: str = Field(
        ...,
        description="Hostname or IP address"
    )
    port: int = Field(
        ...,
        ge=1,
        le=65535,
        description="Port number"
    )
    type: EndpointType = Field(
        default=EndpointType.PRIMARY,
        description="Endpoint type (primary/replica/load_balancer)"
    )
    region: Optional[str] = Field(
        default=None,
        description="Geographic region or availability zone"
    )
    priority: int = Field(
        default=100,
        ge=0,
        le=1000,
        description="Priority for endpoint selection (higher = preferred). Default: 100"
    )
    description: Optional[str] = Field(
        default=None,
        max_length=200,
        description="Human-readable description"
    )
    enabled: bool = Field(
        default=True,
        description="Whether this endpoint is currently enabled"
    )

    @property
    def connection_string(self) -> str:
        """Get formatted connection string."""
        return f"{self.host}:{self.port}"

    def to_dict(self) -> dict:
        """Convert to dictionary for API response."""
        return {
            "host": self.host,
            "port": self.port,
            "type": self.type.value,
            "region": self.region,
            "priority": self.priority,
            "description": self.description,
            "enabled": self.enabled,
            "connection_string": self.connection_string,
        }
