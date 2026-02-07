import secrets
from typing import Optional

from fastapi import Depends, Header, HTTPException, status

from .config import get_settings, Settings


async def require_api_key(
    x_api_key: Optional[str] = Header(None, alias="X-API-Key"),
    settings: Settings = Depends(get_settings),
):
    """Validate API key. If no keys configured (dev mode), auth is skipped."""
    allowed_keys = settings.get_api_key_list()
    if not allowed_keys:
        return

    if not x_api_key:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="API key required. Pass X-API-Key header.",
        )

    for key in allowed_keys:
        if secrets.compare_digest(x_api_key, key):
            return

    raise HTTPException(
        status_code=status.HTTP_401_UNAUTHORIZED,
        detail="Invalid API key",
    )
