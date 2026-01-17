"""pg-tikv PostgreSQL client for user management."""

import os
import subprocess
from dataclasses import dataclass, field
from typing import List, Optional, Tuple


@dataclass
class UserInfo:
    """User information from pg_roles."""
    
    name: str
    is_superuser: bool
    can_login: bool
    can_create_db: bool
    can_create_role: bool
    roles: List[str] = field(default_factory=list)


class PgTikvClient:
    """Client for pg-tikv user management operations.
    
    Uses psql subprocess for SQL execution. This approach:
    - Avoids psycopg2 dependency issues
    - Works with pg-tikv's custom authentication
    - Is simple and reliable
    """
    
    def __init__(self, host: str, port: int):
        """Initialize pg-tikv client.
        
        Args:
            host: pg-tikv server host
            port: pg-tikv server port
        """
        self.host = host
        self.port = port
    
    def _run_sql(
        self,
        tenant: str,
        user: str,
        password: str,
        sql: str
    ) -> Tuple[str, str, int]:
        """Execute SQL via psql subprocess.
        
        Args:
            tenant: Tenant name
            user: Username
            password: Password
            sql: SQL to execute
        
        Returns:
            Tuple of (stdout, stderr, return_code)
        """
        connect_user = f"{tenant}.{user}"
        env = os.environ.copy()
        env["PGPASSWORD"] = password
        
        try:
            result = subprocess.run(
                [
                    "psql",
                    "-h", self.host,
                    "-p", str(self.port),
                    "-U", connect_user,
                    "-d", "postgres",
                    "-t", "-A",  # Tuples only, unaligned
                    "-c", sql
                ],
                capture_output=True,
                text=True,
                env=env,
                timeout=30,
            )
            return result.stdout.strip(), result.stderr.strip(), result.returncode
        except subprocess.TimeoutExpired:
            return "", "Command timed out", 1
        except FileNotFoundError:
            return "", "psql not found", 1
    
    def execute_sql(
        self,
        tenant: str,
        user: str,
        password: str,
        sql: str
    ) -> Optional[str]:
        """Execute SQL and return result.
        
        Args:
            tenant: Tenant name
            user: Username
            password: Password
            sql: SQL to execute
        
        Returns:
            Result string or None on error
        """
        stdout, stderr, rc = self._run_sql(tenant, user, password, sql)
        if rc != 0:
            return None
        return stdout
    
    def test_connection(self, tenant: str, user: str, password: str) -> bool:
        """Test if credentials are valid.
        
        Args:
            tenant: Tenant name
            user: Username
            password: Password
        
        Returns:
            True if connection succeeds
        """
        result = self.execute_sql(tenant, user, password, "SELECT 1")
        return result == "1"
    
    def list_users(
        self,
        tenant: str,
        admin_user: str,
        admin_password: str
    ) -> List[UserInfo]:
        """List all users in the tenant.
        
        Args:
            tenant: Tenant name
            admin_user: Admin username
            admin_password: Admin password
        
        Returns:
            List of UserInfo objects
        """
        sql = """
            SELECT rolname, rolsuper, rolcanlogin, rolcreatedb, rolcreaterole
            FROM pg_catalog.pg_roles
            WHERE rolname NOT LIKE 'pg_%'
        """
        result = self.execute_sql(tenant, admin_user, admin_password, sql)
        
        if not result:
            # Return at least the admin user if query fails
            return [UserInfo(
                name=admin_user,
                is_superuser=True,
                can_login=True,
                can_create_db=True,
                can_create_role=True,
            )]
        
        users = []
        for line in result.split("\n"):
            if not line.strip():
                continue
            parts = line.split("|")
            if len(parts) >= 5:
                users.append(UserInfo(
                    name=parts[0],
                    is_superuser=parts[1].lower() == "t",
                    can_login=parts[2].lower() == "t",
                    can_create_db=parts[3].lower() == "t",
                    can_create_role=parts[4].lower() == "t",
                ))
        
        return users if users else [UserInfo(
            name=admin_user,
            is_superuser=True,
            can_login=True,
            can_create_db=True,
            can_create_role=True,
        )]
    
    def create_user(
        self,
        tenant: str,
        admin_user: str,
        admin_password: str,
        new_user: str,
        new_password: str,
        superuser: bool = False
    ) -> bool:
        """Create a new user in the tenant.
        
        Args:
            tenant: Tenant name
            admin_user: Admin username
            admin_password: Admin password
            new_user: New username
            new_password: New user's password
            superuser: Grant superuser privileges
        
        Returns:
            True if created successfully
        """
        options = "SUPERUSER" if superuser else ""
        # Escape single quotes in password
        escaped_password = new_password.replace("'", "''")
        sql = f"CREATE ROLE {new_user} WITH LOGIN PASSWORD '{escaped_password}' {options}"
        result = self.execute_sql(tenant, admin_user, admin_password, sql)
        return result is not None
    
    def drop_user(
        self,
        tenant: str,
        admin_user: str,
        admin_password: str,
        username: str
    ) -> bool:
        """Delete a user from the tenant.
        
        Args:
            tenant: Tenant name
            admin_user: Admin username
            admin_password: Admin password
            username: User to delete
        
        Returns:
            True if deleted successfully
        """
        sql = f"DROP ROLE IF EXISTS {username}"
        result = self.execute_sql(tenant, admin_user, admin_password, sql)
        return result is not None
    
    def reset_password(
        self,
        tenant: str,
        admin_user: str,
        admin_password: str,
        target_user: str,
        new_password: str
    ) -> bool:
        """Reset a user's password.
        
        Args:
            tenant: Tenant name
            admin_user: Admin username
            admin_password: Admin password
            target_user: User whose password to reset
            new_password: New password
        
        Returns:
            True if reset successfully
        """
        # Escape single quotes in password
        escaped_password = new_password.replace("'", "''")
        sql = f"ALTER ROLE {target_user} WITH PASSWORD '{escaped_password}'"
        result = self.execute_sql(tenant, admin_user, admin_password, sql)
        return result is not None

    def bootstrap_admin_password(
        self,
        tenant: str,
        admin_user: str,
        new_password: str,
        default_password: str = "admin",
        max_retries: int = 5
    ) -> bool:
        """Set admin password for a newly created tenant.
        
        When a new keyspace is created, pg-tikv bootstraps an admin user
        with default password "admin" on first connection. This method:
        1. Connects with default password to trigger bootstrap
        2. Changes password to the desired value
        
        Args:
            tenant: Tenant name (keyspace)
            admin_user: Admin username (typically "admin")
            new_password: Desired admin password
            default_password: Default bootstrap password (default: "admin")
            max_retries: Number of connection attempts (keyspace creation may take time)
        
        Returns:
            True if password was set successfully
        """
        import time
        
        for attempt in range(max_retries):
            if self.test_connection(tenant, admin_user, default_password):
                break
            time.sleep(0.5)
        else:
            return False
        
        escaped_password = new_password.replace("'", "''")
        sql = f"ALTER ROLE {admin_user} WITH PASSWORD '{escaped_password}'"
        result = self.execute_sql(tenant, admin_user, default_password, sql)
        return result is not None
