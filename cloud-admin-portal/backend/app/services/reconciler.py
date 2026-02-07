import asyncio
import logging
from datetime import datetime, timezone, timedelta

from sqlalchemy.orm import Session

from ..models.db import TenantDB
from .pd_client import PDClient
from .tenant import is_tenant_keyspace, parse_keyspace

log = logging.getLogger(__name__)


class Reconciler:
    def __init__(self, pd: PDClient, db_session_factory, interval_seconds: int = 300):
        self._pd = pd
        self._db_factory = db_session_factory
        self._interval = interval_seconds
        self._running = False
        self._cursor = ""

    async def start(self):
        self._running = True
        log.info("Reconciler started (interval=%ds)", self._interval)

        await asyncio.sleep(10)

        while self._running:
            try:
                self._tick()
            except Exception as e:
                log.error("Reconciler tick failed: %s", e)

            await asyncio.sleep(self._interval)

    def stop(self):
        self._running = False
        log.info("Reconciler stopped")

    def _tick(self):
        db: Session = self._db_factory()
        try:
            self._recover_stuck_tenants(db)
            self._incremental_sweep(db)
            db.commit()
        except Exception:
            db.rollback()
            raise
        finally:
            db.close()

    def _recover_stuck_tenants(self, db: Session):
        cutoff = datetime.now(timezone.utc) - timedelta(minutes=10)
        stuck = db.query(TenantDB).filter(
            TenantDB.state.in_(("CREATING", "DISABLING")),
            TenantDB.created_at < cutoff,
        ).all()

        for tenant in stuck:
            ks = self._pd.get_keyspace(tenant.keyspace)
            if tenant.state == "CREATING":
                if ks:
                    tenant.state = "ACTIVE"
                    tenant.state_reason = "Recovered by reconciler: keyspace exists"
                    log.info("Recovered stuck CREATING tenant %s -> ACTIVE", tenant.id)
                else:
                    tenant.state = "CREATE_FAILED"
                    tenant.state_reason = "Recovered by reconciler: keyspace not found after timeout"
                    log.info("Recovered stuck CREATING tenant %s -> CREATE_FAILED", tenant.id)
            elif tenant.state == "DISABLING":
                if ks is None or (isinstance(ks, dict) and ks.get("state") == "DISABLED"):
                    tenant.state = "DISABLED"
                    tenant.state_reason = "Recovered by reconciler"
                    log.info("Recovered stuck DISABLING tenant %s -> DISABLED", tenant.id)
                else:
                    self._pd.disable_keyspace(tenant.keyspace)
                    tenant.state = "DISABLED"
                    tenant.state_reason = "Recovered by reconciler: force-disabled"
                    log.info("Force-disabled stuck DISABLING tenant %s", tenant.id)

            tenant.updated_at = datetime.now(timezone.utc)

    def _incremental_sweep(self, db: Session):
        batch = (
            db.query(TenantDB)
            .filter(TenantDB.state == "ACTIVE", TenantDB.id > self._cursor)
            .order_by(TenantDB.id)
            .limit(100)
            .all()
        )

        if not batch:
            self._cursor = ""
            return

        for tenant in batch:
            ks = self._pd.get_keyspace(tenant.keyspace)
            if ks is None:
                log.warning(
                    "Tenant %s (keyspace=%s) is ACTIVE in DB but missing from PD",
                    tenant.id, tenant.keyspace,
                )

        self._cursor = batch[-1].id

    def sync_new_keyspaces(self, db: Session):
        tikv_keyspaces = self._pd.list_keyspaces()
        tikv_tenant_keyspaces = {}

        for ks in tikv_keyspaces:
            name = ks.get("name", "") if isinstance(ks, dict) else str(ks)
            if name and is_tenant_keyspace(name):
                parsed = parse_keyspace(name)
                if parsed:
                    tenant_id, _ = parsed
                    tikv_tenant_keyspaces[name] = tenant_id

        db_keyspaces = {t.keyspace for t in db.query(TenantDB).all()}
        new_keyspaces = set(tikv_tenant_keyspaces.keys()) - db_keyspaces
        created_count = 0

        for keyspace in new_keyspaces:
            tenant_id = tikv_tenant_keyspaces[keyspace]
            existing = db.query(TenantDB).filter(TenantDB.id == tenant_id).first()
            if existing:
                continue
            db.add(TenantDB(
                id=tenant_id,
                keyspace=keyspace,
                state="ACTIVE",
                created_at=datetime.now(timezone.utc),
                created_by="reconciler_sync",
            ))
            created_count += 1

        if created_count > 0:
            log.info("Reconciler synced %d new keyspaces from PD", created_count)

        return created_count
