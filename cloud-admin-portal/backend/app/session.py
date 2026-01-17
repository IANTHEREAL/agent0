"""Tenant session management.

Tenant sessions store validated credentials for user management operations.
This avoids passing passwords in URLs and reduces credential exposure.
"""

from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from typing import Dict, Optional
import secrets

from .config import get_settings


@dataclass
class TenantSession:
    """Stores validated tenant credentials for user management operations."""
    
    session_id: str
    tenant_name: str
    admin_user: str
    admin_password: str
    created_at: datetime
    expires_at: datetime
    
    def is_expired(self) -> bool:
        """Check if session has expired."""
        return datetime.now(timezone.utc) > self.expires_at


class SessionManager:
    """Manages tenant sessions with automatic cleanup.
    
    In production, this should be replaced with Redis for horizontal scaling.
    """
    
    def __init__(self):
        self._sessions: Dict[str, TenantSession] = {}
    
    def create_session(
        self,
        tenant_name: str,
        admin_user: str,
        admin_password: str
    ) -> TenantSession:
        """Create a new tenant session.
        
        Args:
            tenant_name: Name of the tenant
            admin_user: Admin username for the tenant
            admin_password: Admin password for the tenant
        
        Returns:
            Created TenantSession
        """
        settings = get_settings()
        
        # Generate unique session ID
        session_id = f"ts_{secrets.token_hex(16)}"
        
        now = datetime.now(timezone.utc)
        expires_at = now + timedelta(hours=settings.session_ttl_hours)
        
        session = TenantSession(
            session_id=session_id,
            tenant_name=tenant_name,
            admin_user=admin_user,
            admin_password=admin_password,
            created_at=now,
            expires_at=expires_at,
        )
        
        self._sessions[session_id] = session
        
        # Cleanup expired sessions periodically
        self._cleanup_expired()
        
        return session
    
    def get_session(self, session_id: str) -> Optional[TenantSession]:
        """Get a session by ID.
        
        Args:
            session_id: Session ID
        
        Returns:
            TenantSession if found and not expired, None otherwise
        """
        session = self._sessions.get(session_id)
        
        if session is None:
            return None
        
        if session.is_expired():
            del self._sessions[session_id]
            return None
        
        return session
    
    def validate_session(self, session_id: str, tenant_name: str) -> Optional[TenantSession]:
        """Validate a session belongs to the specified tenant.
        
        Args:
            session_id: Session ID
            tenant_name: Expected tenant name
        
        Returns:
            TenantSession if valid, None otherwise
        """
        session = self.get_session(session_id)
        
        if session is None:
            return None
        
        if session.tenant_name != tenant_name:
            return None
        
        return session
    
    def delete_session(self, session_id: str) -> bool:
        """Delete a session.
        
        Args:
            session_id: Session ID
        
        Returns:
            True if session was deleted, False if not found
        """
        if session_id in self._sessions:
            del self._sessions[session_id]
            return True
        return False
    
    def clear_all(self):
        """Clear all sessions. Called on shutdown."""
        self._sessions.clear()
    
    def _cleanup_expired(self):
        """Remove expired sessions."""
        now = datetime.now(timezone.utc)
        expired = [
            sid for sid, session in self._sessions.items()
            if session.expires_at < now
        ]
        for sid in expired:
            del self._sessions[sid]


# Global session manager instance
session_manager = SessionManager()
