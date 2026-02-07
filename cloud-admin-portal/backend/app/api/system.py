from fastapi import APIRouter, Depends

from ..models import HealthResponse
from ..services import PDClient
from .deps import get_pd_client


router = APIRouter()


@router.get("/health", response_model=HealthResponse)
async def health_check(pd: PDClient = Depends(get_pd_client)):
    pd_healthy = pd.check_health()
    return HealthResponse(
        status="healthy" if pd_healthy else "degraded",
        pd_healthy=pd_healthy,
    )


@router.get("/info")
async def api_info():
    return {
        "name": "pg-tikv Admin API",
        "version": "2.0.0",
        "docs": "/api/docs",
    }
