import asyncio
import logging
from contextlib import asynccontextmanager

from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware

from .api import create_api_router
from .config import get_settings
from .database import get_db_manager
from .services import PDClient
from .services.reconciler import Reconciler
from .session import session_manager

log = logging.getLogger(__name__)


@asynccontextmanager
async def lifespan(app: FastAPI):
    settings = get_settings()

    db_manager = get_db_manager(settings)
    db_manager.create_tables()
    log.info("Database initialized")

    reconciler_task = None
    reconciler = None
    if settings.reconciler_enabled:
        pd = PDClient(settings.pd_endpoints)
        reconciler = Reconciler(
            pd=pd,
            db_session_factory=db_manager.get_session,
            interval_seconds=settings.reconciler_interval_seconds,
        )

        db_session = db_manager.get_session()
        try:
            count = reconciler.sync_new_keyspaces(db_session)
            db_session.commit()
            if count:
                log.info("Initial sync: %d new keyspaces from PD", count)
        except Exception as e:
            log.warning("Initial keyspace sync failed (non-blocking): %s", e)
            db_session.rollback()
        finally:
            db_session.close()

        reconciler_task = asyncio.create_task(reconciler.start())

    auth_mode = "API Key" if settings.get_api_key_list() else "disabled (dev mode)"
    print(f"""
================================================================================
                      pg-tikv Admin API Server v2.0
================================================================================
  API URL:      http://{settings.api_host}:{settings.api_port}/api
  API Docs:     http://{settings.api_host}:{settings.api_port}/api/docs
  Auth:         {auth_mode}
  Reconciler:   {"on" if settings.reconciler_enabled else "off"}

  PD Endpoints: {settings.pd_endpoints}
  PG Host:      {settings.pg_host}
  PG Port:      {settings.pg_port}
  Database:     {settings.database_url}
================================================================================
""")

    yield

    if reconciler_task and reconciler:
        reconciler.stop()
        reconciler_task.cancel()
        try:
            await reconciler_task
        except asyncio.CancelledError:
            pass

    session_manager.clear_all()


def create_app() -> FastAPI:
    settings = get_settings()

    app = FastAPI(
        title="pg-tikv Admin API",
        version="2.0.0",
        docs_url="/api/docs",
        redoc_url="/api/redoc",
        openapi_url="/api/openapi.json",
        lifespan=lifespan,
    )

    app.add_middleware(
        CORSMiddleware,
        allow_origins=settings.cors_origins,
        allow_credentials=True,
        allow_methods=["GET", "POST", "PUT", "DELETE", "OPTIONS"],
        allow_headers=["Authorization", "X-Tenant-Session", "X-API-Key", "Content-Type"],
    )

    app.include_router(create_api_router())

    return app


app = create_app()


def main():
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
