"""System API endpoints (health, info)."""

from fastapi import APIRouter, Depends

from ..config import get_settings, Settings
from ..models import HealthResponse
from ..services import PDClient


router = APIRouter()


def get_pd_client(settings: Settings = Depends(get_settings)) -> PDClient:
    """Get PD client instance."""
    return PDClient(settings.pd_endpoints)


@router.get(
    "/health",
    response_model=HealthResponse,
    summary="Health check",
    description="Check API and PD health status."
)
async def health_check(pd: PDClient = Depends(get_pd_client)):
    """Check system health."""
    pd_healthy = pd.check_health()
    return HealthResponse(
        status="healthy" if pd_healthy else "degraded",
        pd_healthy=pd_healthy,
    )


@router.get(
    "/info",
    summary="API information",
    description="Get API version and information."
)
async def api_info():
    """Get API information."""
    return {
        "name": "pg-tikv Admin API",
        "version": "2.0.0",
        "docs": "/api/docs",
    }
