from datetime import datetime, timezone
from typing import List, Optional

from sqlalchemy.orm import Session

from ..models.db import TenantDB
from .pd_client import PDClient
from .tenant import is_tenant_keyspace, parse_keyspace


def sync_tenants_on_startup(pd: PDClient, db: Session) -> dict:
    """Sync TiKV keyspaces with database on startup."""
    tikv_keyspaces = pd.list_keyspaces()
    tikv_tenant_keyspaces = {}

    for ks in tikv_keyspaces:
        if isinstance(ks, dict):
            keyspace = ks.get("name", "")
        else:
            keyspace = str(ks)

        if keyspace and is_tenant_keyspace(keyspace):
            parsed = parse_keyspace(keyspace)
            if parsed:
                tenant_id, _ = parsed
                tikv_tenant_keyspaces[keyspace] = tenant_id

    db_tenants = db.query(TenantDB).all()
    db_keyspaces = {t.keyspace for t in db_tenants}

    new_keyspaces = set(tikv_tenant_keyspaces.keys()) - db_keyspaces
    created_count = 0

    for keyspace in new_keyspaces:
        tenant_id = tikv_tenant_keyspaces[keyspace]
        tenant = TenantDB(
            id=tenant_id,
            keyspace=keyspace,
            is_deleted=False,
            created_at=datetime.now(timezone.utc),
            created_by="system_sync",
            notes="Auto-created during system startup sync",
        )
        db.add(tenant)
        created_count += 1

    if created_count > 0:
        db.commit()

    return {
        "tikv_keyspaces": len(tikv_tenant_keyspaces),
        "db_tenants": len(db_keyspaces),
        "new_tenants_synced": created_count,
        "total_after_sync": len(db_keyspaces) + created_count,
    }


def get_active_tenants(db: Session) -> List[TenantDB]:
    return db.query(TenantDB).filter(TenantDB.is_deleted == False).all()


def soft_delete_tenant(tenant_id: str, db: Session) -> bool:
    tenant = db.query(TenantDB).filter(TenantDB.id == tenant_id).first()

    if not tenant or tenant.is_deleted:
        return False

    tenant.is_deleted = True
    tenant.updated_at = datetime.now(timezone.utc)
    db.commit()

    return True


def get_tenant_by_id(tenant_id: str, db: Session, include_deleted: bool = False) -> Optional[TenantDB]:
    query = db.query(TenantDB).filter(TenantDB.id == tenant_id)

    if not include_deleted:
        query = query.filter(TenantDB.is_deleted == False)

    return query.first()
