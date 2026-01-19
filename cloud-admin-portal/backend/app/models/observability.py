"""Observability models for pg-tikv tenant metrics."""

from typing import List

from pydantic import BaseModel, Field


class ObservabilitySummary(BaseModel):
    """Per-tenant summary metrics (rolling 1-hour window)."""

    window_seconds: int = Field(..., ge=1, le=3600, description="Effective window size in seconds")
    statement_count: int = Field(..., ge=0)
    txn_commit_count: int = Field(..., ge=0)
    error_count: int = Field(..., ge=0)

    qps: float = Field(..., ge=0)
    tps: float = Field(..., ge=0)
    latency_avg_ms: float = Field(..., ge=0)
    latency_p99_ms: float = Field(..., ge=0)
    active_connections: int = Field(..., ge=0)


class QuerySample(BaseModel):
    """Sampled SQL statement group (rolling 1-hour window)."""

    query: str
    sample_count: int = Field(..., ge=0)
    error_count: int = Field(..., ge=0)
    latency_avg_ms: float = Field(..., ge=0)
    latency_p99_ms: float = Field(..., ge=0)
    latency_max_ms: float = Field(..., ge=0)
    last_seen_ms_ago: int = Field(..., ge=0)


class TenantObservabilityResponse(BaseModel):
    summary: ObservabilitySummary
    samples: List[QuerySample]

