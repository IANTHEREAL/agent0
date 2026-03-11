# Authentication & RBAC

db9-server implements PostgreSQL-compatible authentication and role-based access control (RBAC).

## Overview

- **Password Authentication**: Cleartext password authentication
- **Token/Connect-key Authentication**: Treat pgwire `PasswordMessage` as DB9 auth material (JWT connect-token or `db9ck_` connect-key)
- **Per-Keyspace Users**: Each keyspace has its own user database
- **RBAC**: Role-based access control with privileges on tables
- **Superuser**: Full access to all operations
- **Bootstrap User**: First superuser created via explicit bootstrap env vars

## Auth Modes (`DB9_AUTH_MODE`)

`DB9_AUTH_MODE` controls what db9-server expects in the pgwire “password” field:

- `password` (default): legacy password authentication.
- `both`: password auth + token/connect-key auth (if the provided secret looks like a JWT / `db9ck_` connect-key, it must validate; no fallback to password on validation failure).
- `token`: token/connect-key only (password auth is rejected).

When token auth is enabled (`both` / `token`), db9-server requires TLS unless `DB9_DEV=1` or `DB9_INSECURE=1`.

### Security & Migration Notes

- `DB9_AUTH_MODE=both` requires TLS for **all** pgwire connections (including legacy password clients): the server cannot know whether the client will send a token or a password until it receives the auth secret.
- In `both` mode, passwords that look like tokens (JWT-like strings or any password starting with `db9ck_`) will be treated as token material; if token validation fails, there is **no fallback** to password auth. Rotate such passwords before enabling `both`.
- fs9 WebSocket authentication follows the same TLS requirement when token auth is enabled.

## Token Authentication (psql)

JWT connect-token (JWKS URL preferred):

```bash
export DB9_AUTH_MODE=token
export DB9_AUTH_JWKS_URL=https://example.com/.well-known/jwks.json
# Optional: set if your tokens use a non-RS256 alg (comma-separated, e.g. RS384,ES256)
# export DB9_AUTH_JWT_ALGORITHM=RS256

PGPASSWORD="<JWT_CONNECT_TOKEN>" psql -h 127.0.0.1 -p 5433 -U "<tenant>.admin" -d postgres
```

Connect-key (introspection):

```bash
export DB9_AUTH_MODE=token
export DB9_AUTH_CONNECT_KEY_INTROSPECT_URL=https://example.com/internal/connect-keys/introspect

PGPASSWORD="db9ck_<connect_key>" psql -h 127.0.0.1 -p 5433 -U "<tenant>.admin" -d postgres
```

## Default User

There is **no implicit default password** in non-dev mode. When a keyspace has no superuser yet, bootstrap the initial superuser by setting:
- `DB9_BOOTSTRAP_ADMIN_PASSWORD` (required)
- `DB9_BOOTSTRAP_ADMIN_USER` (optional; default `admin`)

Then connect as `<keyspace>.<user>` (or `user` for the default keyspace).

**Dev-only**: `DB9_DEV=1` enables legacy insecure bootstrap behavior intended for local development only.

**Important**: In production, enable TLS (`PG_TLS_CERT` + `PG_TLS_KEY`) and consider setting `PG_REQUIRE_TLS=1`.

Change the admin password using:

```sql
ALTER ROLE admin WITH PASSWORD 'your_secure_password';
```

## Creating Users

Note: `PASSWORD` DDL is rejected when `DB9_AUTH_MODE=token`.

### Basic User

```sql
CREATE ROLE username WITH PASSWORD 'password' LOGIN;
```

### User with Options

```sql
CREATE ROLE username WITH 
    PASSWORD 'password' 
    LOGIN 
    CREATEDB 
    CREATEROLE;
```

### Superuser

```sql
CREATE ROLE admin_user WITH 
    PASSWORD 'password' 
    LOGIN 
    SUPERUSER;
```

### Role Options

| Option | Description |
|--------|-------------|
| `PASSWORD 'xxx'` | Set password |
| `LOGIN` | Allow login (required for connecting) |
| `NOLOGIN` | Disallow login (for roles only) |
| `SUPERUSER` | Grant all privileges |
| `NOSUPERUSER` | Normal user (default) |
| `CREATEDB` | Allow creating databases |
| `NOCREATEDB` | Disallow creating databases (default) |
| `CREATEROLE` | Allow creating other roles |
| `NOCREATEROLE` | Disallow creating roles (default) |

## Modifying Users

### Change Password

```sql
ALTER ROLE username WITH PASSWORD 'new_password';
```

### Grant/Revoke Options

```sql
ALTER ROLE username WITH SUPERUSER;
ALTER ROLE username WITH NOSUPERUSER;
ALTER ROLE username WITH CREATEDB;
ALTER ROLE username WITH NOLOGIN;
```

### Rename User

```sql
ALTER ROLE old_name RENAME TO new_name;
```

## Deleting Users

```sql
DROP ROLE username;
DROP ROLE IF EXISTS username;
```

## Privileges

### Privilege Types

| Privilege | Applies To | Description |
|-----------|------------|-------------|
| `SELECT` | Tables | Read data |
| `INSERT` | Tables | Insert rows |
| `UPDATE` | Tables | Modify rows |
| `DELETE` | Tables | Delete rows |
| `TRUNCATE` | Tables | Truncate table |
| `REFERENCES` | Tables | Create foreign keys |
| `TRIGGER` | Tables | Create triggers |
| `CREATE` | Schemas | Create objects |
| `CONNECT` | Databases | Connect to database |
| `USAGE` | Schemas, Sequences | Use schema/sequence |
| `EXECUTE` | Functions | Execute function |
| `ALL` | Any | All applicable privileges |

### Granting Privileges

#### On Specific Table

```sql
GRANT SELECT ON users TO reader;
GRANT SELECT, INSERT ON users TO writer;
GRANT ALL ON users TO admin_user;
```

#### On All Tables in Schema

```sql
GRANT SELECT ON ALL TABLES IN SCHEMA public TO reader;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO app_user;
```

#### With Grant Option

```sql
-- User can grant this privilege to others
GRANT SELECT ON users TO manager WITH GRANT OPTION;
```

### Revoking Privileges

```sql
REVOKE DELETE ON users FROM writer;
REVOKE ALL ON users FROM temp_user;
REVOKE SELECT ON ALL TABLES IN SCHEMA public FROM reader;
```

## Roles (Groups)

Roles can be used as groups to manage privileges for multiple users.

### Create Role Group

```sql
-- Create a role without login (group)
CREATE ROLE readonly NOLOGIN;
GRANT SELECT ON ALL TABLES IN SCHEMA public TO readonly;

-- Create users and add to group
CREATE ROLE user1 WITH PASSWORD 'pass1' LOGIN;
CREATE ROLE user2 WITH PASSWORD 'pass2' LOGIN;

-- Add users to group (grant role membership)
ALTER ROLE readonly ADD MEMBER user1;
ALTER ROLE readonly ADD MEMBER user2;
```

### Remove from Group

```sql
ALTER ROLE readonly DROP MEMBER user1;
```

## Common Patterns

### Read-Only User

```sql
CREATE ROLE readonly WITH PASSWORD 'readonly_pass' LOGIN;
GRANT SELECT ON ALL TABLES IN SCHEMA public TO readonly;
```

### Application User

```sql
CREATE ROLE app_user WITH PASSWORD 'app_pass' LOGIN;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO app_user;
```

### Admin User

```sql
CREATE ROLE db_admin WITH PASSWORD 'admin_pass' LOGIN SUPERUSER CREATEDB CREATEROLE;
```

### Analytics User

```sql
CREATE ROLE analyst WITH PASSWORD 'analyst_pass' LOGIN;
GRANT SELECT ON ALL TABLES IN SCHEMA public TO analyst;
-- Optionally grant specific tables
GRANT SELECT ON orders TO analyst;
GRANT SELECT ON users TO analyst;
```

## Multi-Tenant User Management

Each keyspace has its own user database. Users must be created per keyspace.

### Example: Two Tenants

```bash
# Connect to tenant_a
psql -h 127.0.0.1 -p 5433 -U tenant_a.admin
```

```sql
-- Create tenant_a users
CREATE ROLE app WITH PASSWORD 'tenant_a_app_pass' LOGIN;
GRANT SELECT, INSERT, UPDATE ON ALL TABLES IN SCHEMA public TO app;
```

```bash
# Connect to tenant_b
psql -h 127.0.0.1 -p 5433 -U tenant_b.admin
```

```sql
-- Create tenant_b users (completely separate)
CREATE ROLE app WITH PASSWORD 'tenant_b_app_pass' LOGIN;
GRANT SELECT, INSERT, UPDATE ON ALL TABLES IN SCHEMA public TO app;
```

## Security Best Practices

### 1. Change Default Password

```sql
ALTER ROLE admin WITH PASSWORD 'strong_random_password_here';
```

### 2. Use Strong Passwords

```sql
-- Good: Long, random
CREATE ROLE app WITH PASSWORD 'xK9#mP2$vL5@nQ8&' LOGIN;

-- Bad: Weak
CREATE ROLE app WITH PASSWORD 'password123' LOGIN;
```

### 3. Principle of Least Privilege

```sql
-- Only grant what's needed
GRANT SELECT ON orders TO reporting_user;
-- NOT: GRANT ALL ON ALL TABLES TO reporting_user;
```

### 4. Separate Users Per Application

```sql
-- Each service gets its own user
CREATE ROLE api_service WITH PASSWORD 'pass1' LOGIN;
CREATE ROLE worker_service WITH PASSWORD 'pass2' LOGIN;
CREATE ROLE admin_service WITH PASSWORD 'pass3' LOGIN;
```

### 5. Audit User Creation

Keep track of all users and their privileges:

```sql
-- List all users (future feature)
-- Currently users are stored in TiKV, 
-- use CREATE ROLE statements as documentation
```

## Limitations

1. **Password Storage**: Passwords are hashed with SHA256, but transmitted in cleartext (use TLS in production)
2. **No Row-Level Security**: Cannot restrict access to specific rows
3. **No Column-Level Privileges**: Privileges are at table level
4. **No GRANT ON DATABASE**: All tables are in a single logical database
5. **No pg_catalog**: System catalogs are not implemented
