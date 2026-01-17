"""API route handlers."""

from fastapi import APIRouter

from .tenants import router as tenants_router
from .users import router as users_router
from .system import router as system_router
from .audit import router as audit_router


def create_api_router() -> APIRouter:
    """Create the main API router with all sub-routers."""
    router = APIRouter(prefix="/api")

    router.include_router(tenants_router, prefix="/tenants", tags=["Tenants"])
    router.include_router(users_router, tags=["Users"])
    router.include_router(system_router, tags=["System"])
    router.include_router(audit_router, prefix="/audit-logs", tags=["Audit"])

    return router
