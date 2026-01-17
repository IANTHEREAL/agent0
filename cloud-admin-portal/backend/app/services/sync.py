"""Tenant synchronization service for TiKV/Database consistency."""

from datetime import datetime, timezone
from typing import List

from sqlalchemy.orm import Session

from ..models.db import TenantDB
from .pd_client import PDClient


def sync_tenants_on_startup(pd: PDClient, db: Session) -> dict:
    """Sync TiKV keyspaces with database on startup.

    This ensures consistency between TiKV keyspaces and portal database:
    - Tenants in TiKV but not in DB -> Create DB record
    - Tenants in DB but not in TiKV -> Keep DB record (manual cleanup)

    Args:
        pd: PD client for TiKV keyspace management
        db: Database session

    Returns:
        Dictionary with sync statistics
    """
    # Get all keyspaces from TiKV
    tikv_keyspaces = pd.list_keyspaces()
    tikv_names = set()

    for ks in tikv_keyspaces:
        if isinstance(ks, dict):
            name = ks.get("name", "")
        else:
            name = str(ks)

        # Skip system keyspaces
        if name and not name.startswith("_"):
            tikv_names.add(name)

    # Get all tenants from database
    db_tenants = db.query(TenantDB).all()
    db_names = {t.name for t in db_tenants}

    # Find tenants that exist in TiKV but not in database
    new_tenants = tikv_names - db_names
    created_count = 0

    for tenant_name in new_tenants:
        tenant = TenantDB(
            name=tenant_name,
            is_deleted=False,
            created_at=datetime.now(timezone.utc),
            created_by="system_sync",  # Mark as system-created
            notes=f"Auto-created during system startup sync",
        )
        db.add(tenant)
        created_count += 1

    if created_count > 0:
        db.commit()

    return {
        "tikv_keyspaces": len(tikv_names),
        "db_tenants": len(db_names),
        "new_tenants_synced": created_count,
        "total_after_sync": len(db_names) + created_count,
    }


def get_active_tenants(db: Session) -> List[TenantDB]:
    """Get all active (non-deleted) tenants from database.

    Args:
        db: Database session

    Returns:
        List of active tenant records
    """
    return db.query(TenantDB).filter(TenantDB.is_deleted == False).all()


def soft_delete_tenant(tenant_name: str, db: Session) -> bool:
    """Soft delete a tenant by marking it as deleted.

    Args:
        tenant_name: Name of tenant to delete
        db: Database session

    Returns:
        True if tenant was found and marked deleted, False otherwise
    """
    tenant = db.query(TenantDB).filter(TenantDB.name == tenant_name).first()

    if not tenant or tenant.is_deleted:
        return False

    tenant.is_deleted = True
    tenant.updated_at = datetime.now(timezone.utc)
    db.commit()

    return True


def get_tenant_by_name(tenant_name: str, db: Session, include_deleted: bool = False) -> TenantDB:
    """Get a tenant by name.

    Args:
        tenant_name: Name of tenant to get
        db: Database session
        include_deleted: If True, include soft-deleted tenants

    Returns:
        Tenant record or None if not found
    """
    query = db.query(TenantDB).filter(TenantDB.name == tenant_name)

    if not include_deleted:
        query = query.filter(TenantDB.is_deleted == False)

    return query.first()
