from datetime import datetime, timezone
from typing import List, Optional

from sqlalchemy.orm import Session

from ..models.db import TenantDB


def get_active_tenants(db: Session) -> List[TenantDB]:
    return db.query(TenantDB).filter(TenantDB.state == "ACTIVE").all()


def get_tenant_by_id(tenant_id: str, db: Session, include_deleted: bool = False) -> Optional[TenantDB]:
    query = db.query(TenantDB).filter(TenantDB.id == tenant_id)
    if not include_deleted:
        query = query.filter(TenantDB.state.notin_(("DISABLED", "CREATE_FAILED")))
    return query.first()
