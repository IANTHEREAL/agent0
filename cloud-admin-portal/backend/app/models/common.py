"""Common response models."""

from typing import Optional, Any

from pydantic import BaseModel


class MessageResponse(BaseModel):
    """Generic message response."""
    
    message: str


class HealthResponse(BaseModel):
    """Health check response."""
    
    status: str
    pd_healthy: bool


class ErrorResponse(BaseModel):
    """Error response."""
    
    error: str
    message: str
    details: Optional[Any] = None


class SqlQueryRequest(BaseModel):
    """SQL query request."""
    
    sql: str


class SqlQueryResponse(BaseModel):
    """SQL query response."""
    
    success: bool
    result: Optional[str] = None
    error: Optional[str] = None
    rows_affected: Optional[int] = None
