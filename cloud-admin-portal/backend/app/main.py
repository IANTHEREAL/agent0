"""FastAPI application entry point."""

from contextlib import asynccontextmanager

from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware

from .api import create_api_router
from .config import get_settings
from .session import session_manager


@asynccontextmanager
async def lifespan(app: FastAPI):
    """Application lifespan handler."""
    # Startup
    settings = get_settings()
    print(f"""
================================================================================
                      pg-tikv Admin API Server v2.0
================================================================================
  API URL:     http://{settings.api_host}:{settings.api_port}/api
  API Docs:    http://{settings.api_host}:{settings.api_port}/api/docs
  
  PD Endpoints: {settings.pd_endpoints}
  PG Host:      {settings.pg_host}
  PG Port:      {settings.pg_port}
================================================================================
""")
    yield
    # Shutdown: cleanup sessions
    session_manager.clear_all()


def create_app() -> FastAPI:
    """Create and configure the FastAPI application."""
    settings = get_settings()
    
    app = FastAPI(
        title="pg-tikv Admin API",
        description="RESTful API for pg-tikv multi-tenant administration",
        version="2.0.0",
        docs_url="/api/docs",
        redoc_url="/api/redoc",
        openapi_url="/api/openapi.json",
        lifespan=lifespan,
    )
    
    # CORS middleware
    app.add_middleware(
        CORSMiddleware,
        allow_origins=settings.cors_origins,
        allow_credentials=True,
        allow_methods=["GET", "POST", "DELETE", "OPTIONS"],
        allow_headers=["Authorization", "X-Tenant-Session", "Content-Type"],
    )
    
    # Include API routes
    app.include_router(create_api_router())
    
    return app


# Create app instance
app = create_app()


def main():
    """Run the API server (for development)."""
    import uvicorn
    
    settings = get_settings()
    uvicorn.run(
        "app.main:app",
        host=settings.api_host,
        port=settings.api_port,
        reload=settings.debug,
    )


if __name__ == "__main__":
    main()
