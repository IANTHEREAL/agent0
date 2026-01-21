"""pg-tikv PostgreSQL client for tenant/user/observability operations.

Use a pure-Python driver (`pg8000`) to avoid spawning `psql` subprocesses.
"""

import time
from contextlib import contextmanager
from dataclasses import dataclass, field
from typing import List, Optional, Sequence, Tuple

import pg8000.dbapi as pg_dbapi
import sqlparse

from ..models import ObservabilitySummary, QuerySample


def _quote_ident(ident: str) -> str:
    # Minimal, safe SQL identifier quoting.
    # pg-tikv uses sqlparser, so standard Postgres quoting is expected to work.
    return '"' + ident.replace('"', '""') + '"'


def _format_cell(value: object) -> str:
    if value is None:
        return ""
    if isinstance(value, bool):
        return "t" if value else "f"
    return str(value)


def _format_row_pipe(row: Sequence[object]) -> str:
    return "|".join(_format_cell(v) for v in row)


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
    """Client for pg-tikv operations (users + observability + ad-hoc SQL).

    Uses `pg8000` (pure Python).
    """
    
    def __init__(self, host: str, port: int):
        """Initialize pg-tikv client.
        
        Args:
            host: pg-tikv server host
            port: pg-tikv server port
        """
        self.host = host
        self.port = port
    
    @contextmanager
    def _connect(self, tenant: str, user: str, password: str) -> object:
        connect_user = f"{tenant}.{user}"
        conn = pg_dbapi.connect(
            user=connect_user,
            password=password,
            host=self.host,
            port=self.port,
            database="postgres",
        )
        try:
            conn.autocommit = True  # type: ignore[attr-defined]
        except Exception:
            pass

        try:
            yield conn
        finally:
            try:
                conn.close()  # type: ignore[attr-defined]
            except Exception:
                pass

    def _run_sql(
        self,
        tenant: str,
        user: str,
        password: str,
        sql: str
    ) -> Tuple[str, str, int]:
        """Execute SQL and return a `psql -t -A`-compatible output string.
        
        Supports multi-statement SQL scripts by splitting on semicolons
        using sqlparse for proper handling of strings and comments.
        
        Args:
            tenant: Tenant name
            user: Username
            password: Password
            sql: SQL to execute (can contain multiple statements)
        
        Returns:
            Tuple of (stdout, stderr, return_code)
        """
        statements = sqlparse.split(sql)
        statements = [s.strip() for s in statements if s.strip()]
        
        if not statements:
            return "", "", 0
        
        out_lines: List[str] = []
        try:
            with self._connect(tenant, user, password) as conn:
                cursor = conn.cursor()  # type: ignore[attr-defined]
                
                for stmt in statements:
                    if not stmt or stmt.startswith("--"):
                        continue
                    
                    cursor.execute(stmt)  # type: ignore[attr-defined]
                    
                    if getattr(cursor, "description", None) is not None:
                        rows = cursor.fetchall()  # type: ignore[attr-defined]
                        for row in rows:
                            out_lines.append(_format_row_pipe(row))
                        
                        if out_lines and statements.index(stmt) < len(statements) - 1:
                            out_lines.append("")
                
                try:
                    cursor.close()  # type: ignore[attr-defined]
                except Exception:
                    pass

            return "\n".join(out_lines).strip(), "", 0
        except Exception as e:
            return "", str(e), 1
    
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

    def get_observability_summary(
        self,
        tenant: str,
        user: str,
        password: str,
    ) -> Tuple[Optional[ObservabilitySummary], Optional[str]]:
        sql = """
            SELECT
                window_seconds,
                statement_count,
                txn_commit_count,
                error_count,
                qps,
                tps,
                latency_avg_ms,
                latency_p99_ms,
                active_connections
            FROM _pgtikv_sys_observability()
        """
        stdout, stderr, rc = self._run_sql(tenant, user, password, sql)
        if rc != 0:
            return None, stderr or "Query execution failed"
        if not stdout:
            return None, "Empty response from pg-tikv"

        parts = stdout.split("|")
        if len(parts) < 9:
            return None, f"Unexpected response: {stdout!r}"

        try:
            summary = ObservabilitySummary(
                window_seconds=int(parts[0]),
                statement_count=int(parts[1]),
                txn_commit_count=int(parts[2]),
                error_count=int(parts[3]),
                qps=float(parts[4]),
                tps=float(parts[5]),
                latency_avg_ms=float(parts[6]),
                latency_p99_ms=float(parts[7]),
                active_connections=int(parts[8]),
            )
        except ValueError as e:
            return None, f"Failed to parse pg-tikv response: {e}"

        return summary, None

    def get_observability_samples(
        self,
        tenant: str,
        user: str,
        password: str,
    ) -> Tuple[List[QuerySample], Optional[str]]:
        sql = """
            SELECT
                query,
                sample_count,
                error_count,
                latency_avg_ms,
                latency_p99_ms,
                latency_max_ms,
                last_seen_ms_ago
            FROM _pgtikv_sys_query_samples()
        """
        stdout, stderr, rc = self._run_sql(tenant, user, password, sql)
        if rc != 0:
            return [], stderr or "Query execution failed"
        if not stdout:
            return [], None

        samples: List[QuerySample] = []
        for line in stdout.splitlines():
            if not line.strip():
                continue
            parts = line.split("|")
            if len(parts) < 7:
                continue
            try:
                samples.append(
                    QuerySample(
                        query=parts[0],
                        sample_count=int(parts[1]),
                        error_count=int(parts[2]),
                        latency_avg_ms=float(parts[3]),
                        latency_p99_ms=float(parts[4]),
                        latency_max_ms=float(parts[5]),
                        last_seen_ms_ago=int(parts[6]),
                    )
                )
            except ValueError:
                continue

        return samples, None
    
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
        sql = (
            f"CREATE ROLE {_quote_ident(new_user)} WITH LOGIN PASSWORD '{escaped_password}' {options}"
        )
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
        sql = f"DROP ROLE IF EXISTS {_quote_ident(username)}"
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
        sql = f"ALTER ROLE {_quote_ident(target_user)} WITH PASSWORD '{escaped_password}'"
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
