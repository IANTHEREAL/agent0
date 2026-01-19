import secrets
import string

TENANT_ID_LENGTH = 14
TENANT_PREFIX = "t"
SYSTEM_PREFIX = "s"


def generate_tenant_id() -> str:
    alphabet = string.ascii_lowercase + string.ascii_uppercase + string.digits
    return ''.join(secrets.choice(alphabet) for _ in range(TENANT_ID_LENGTH))


def make_keyspace(tenant_id: str, suffix: str | None = None) -> str:
    """Format: t{id} or t{id}{suffix}"""
    if suffix:
        return f"{TENANT_PREFIX}{tenant_id}{suffix}"
    return f"{TENANT_PREFIX}{tenant_id}"


def parse_keyspace(keyspace: str) -> tuple[str, str | None] | None:
    """Returns (tenant_id, suffix) or None if not a tenant keyspace."""
    if not keyspace.startswith(TENANT_PREFIX):
        return None
    if len(keyspace) < 1 + TENANT_ID_LENGTH:
        return None
    
    tenant_id = keyspace[1:1 + TENANT_ID_LENGTH]
    suffix = keyspace[1 + TENANT_ID_LENGTH:] or None
    return (tenant_id, suffix)


def is_system_keyspace(keyspace: str) -> bool:
    return (
        keyspace.startswith(SYSTEM_PREFIX) or
        keyspace.startswith("_") or
        keyspace == "DEFAULT"
    )


def is_tenant_keyspace(keyspace: str) -> bool:
    return keyspace.startswith(TENANT_PREFIX) and len(keyspace) >= 1 + TENANT_ID_LENGTH
