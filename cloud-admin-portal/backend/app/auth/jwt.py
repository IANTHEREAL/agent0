"""JWT token generation and validation."""

from datetime import datetime, timedelta, timezone
from typing import Optional

import jwt
from jwt.exceptions import ExpiredSignatureError, InvalidTokenError

from ..config import get_settings


def create_access_token(
    subject: str,
    expires_delta: Optional[timedelta] = None
) -> tuple[str, datetime]:
    """Create a JWT access token.
    
    Args:
        subject: Token subject (typically username)
        expires_delta: Custom expiry duration. Uses config default if not provided.
    
    Returns:
        Tuple of (token_string, expiration_datetime)
    """
    settings = get_settings()
    
    if expires_delta is None:
        expires_delta = timedelta(hours=settings.jwt_expiry_hours)
    
    now = datetime.now(timezone.utc)
    expires_at = now + expires_delta
    
    payload = {
        "sub": subject,
        "iat": now,
        "exp": expires_at,
        "type": "access"
    }
    
    token = jwt.encode(payload, settings.jwt_secret, algorithm=settings.jwt_algorithm)
    return token, expires_at


def decode_token(token: str) -> dict:
    """Decode and validate a JWT token.
    
    Args:
        token: JWT token string
    
    Returns:
        Decoded payload dict
    
    Raises:
        ExpiredSignatureError: If token has expired
        InvalidTokenError: If token is invalid
    """
    settings = get_settings()
    
    return jwt.decode(
        token,
        settings.jwt_secret,
        algorithms=[settings.jwt_algorithm]
    )
