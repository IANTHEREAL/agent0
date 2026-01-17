"""API route handlers."""

from fastapi import APIRouter

from .auth import router as auth_router
from .tenants import router as tenants_router
from .users import router as users_router
from .system import router as system_router


def create_api_router() -> APIRouter:
    """Create the main API router with all sub-routers."""
    router = APIRouter(prefix="/api")
    
    router.include_router(auth_router, prefix="/auth", tags=["Authentication"])
    router.include_router(tenants_router, prefix="/tenants", tags=["Tenants"])
    router.include_router(users_router, tags=["Users"])  # Already has /tenants/{name}
    router.include_router(system_router, tags=["System"])
    
    return router
