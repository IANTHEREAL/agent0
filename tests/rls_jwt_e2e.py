#!/usr/bin/env python3
"""
E2E test: JWT token -> auth.uid() -> RLS policy enforcement.

Tests the full chain from a user's perspective:
1. Connect to db9-server using a JWT token (as password)
2. Verify auth.uid() returns the correct JWT sub claim
3. Create a table with RLS policy that uses auth.uid()
4. Verify row-level filtering based on JWT identity

Requirements:
  - db9-server running with DB9_AUTH_MODE=both (or token)
  - DB9_AUTH_JWT_PUBLIC_KEY set to the test RSA public key
  - DB9_AUTH_ISSUER=https://issuer.example
  - DB9_AUTH_AUDIENCE=db9-server
  - pip install psycopg2-binary PyJWT cryptography

Usage:
  python3 tests/rls_jwt_e2e.py --host 127.0.0.1 --port 5433
"""

import argparse
import json
import sys
import time

try:
    import jwt as pyjwt
    import psycopg2
except ImportError:
    print("SKIP: psycopg2-binary and/or PyJWT not installed", file=sys.stderr)
    sys.exit(0)

# Test-only RSA key pair — NOT a real secret.
# Same keys used in db9-server Rust unit tests (src/auth/db9_auth.rs).
TEST_RSA_PRIVATE_KEY = """-----BEGIN PRIVATE KEY-----
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQCvjVk3qWFad3bQ
HsXmiT5i6g3SEDk+VwmfOgNEwBwW/xjpMog9K8RPe3b7S4XSDh8vmDOh20flJQs+
T4QahMtaD75nsG7a3uJAQvBxAsNdlF6r/9swga07/gXl/UaIYYbym7DGNitvOPL3
QfCzcR3yO0ZKGBVfVbYjtQNPc3WbJvPhHWZV+8icY5v6yL9Y0p8p8RxndFdoHHdI
oIJqkSGy6vJ26iHZA5MJ+kTN1AzW0K/yLSll/4+4HWxYQy48RXNKqh6/Kn5HPJ0c
aZKmOHnrWBIisAeION+5lwj6n9HQvMzPJK46TEAMvTevnLWjnYUboKTf6au4+Whv
7+X13REnAgMBAAECggEAA2Qlnw+kk8zO/MI7bHKmQ97lmXM6x9uCkhLa0U8su7z9
zDNvsk7QIgDukXgqA57GN3MnPC8yOlj22KNMl/6MtxaqxPIBkjTQBhHE90noYDxn
f8cXgt5ebFRB5Ol5nVTU+IbNaWbOe/2Lo/8gGTdMLsu6VeAVOZw8QoBSqgw+71pP
EjdjUUNAayE1om/86QlmtK1+9uci7Jam+8Kvy527lIjCQdwR8kT0Kv8AM89EuAGp
FhdnF146YVBYTzR21drUERNh2oCaTzRdRrTYGZICH7qLnhufojI7Qp6QODgr5U1r
Y8UGyC7XpCh4cklP0/FZA0AXqVcY8h0TdMs28ZYPMQKBgQDizkZKKIRW9DPxhZkC
Uy2pju70LhKtLBtBoCDzkZyIVnJtVeQnRpDwMTr5eDFojDWFpwf8VuIpE78QybW+
5rIllszKaOE+B+W8P+zIIKjI21Ag3KOazwrmAq5lplCvT3Rhy36CDpjZSC7LYVqT
Jw1damg8YA9sJdcoA6TkR4ij7QKBgQDGJijl2nHYtTP1wHxOaEK8kNtno1HtbJ8F
csbTicit7s2xfZHy0cKfnxUxsSRa1T8j33g/gbRNQYrrn2G/wAG6G4IlcoDlsg8y
viGh/t3Oo4KdbQbQpwIA0nH46KdGmuZoWOX1u8Bg51BsjpxMSvxY6TYzonq/IK11
1U6zD5fO4wKBgG2QLf5nAj8rKuiSnC62VcmiJabJlvYW53fVTfW7sr1d3VsZ8eRT
P3L4pT+cI2oYyUYuQTpSEmC7jEIk3upAcXCdH4LsFVss33sH+m9W75JP965YR6Ri
PiaMxwiNxk5Z+KPBdPSI7qeQKiLPfby2Uct9uqrn0KtywDQxRneMYuKlAoGAI+aS
DmMvsVXTXjlLzGDzhnqwZeyfUWcWwMP05irWozzbI8dehCIhIw6Npn0z2wk78WHx
xX/YjQ7M/rfX3AgLyA5n3CUM2ZETU9xC97jXszLI3YD9dRxtLnzyjWiJti8mg81n
jMhBqM0AM0r7Yo9LfUhzu5M6rhpbkzfclHDEzoUCgYAeiCs36dfsvLN63aXtyViw
1KuUAX7ZOH/OE9AW4LXZ0SPV5xhaodEm+MiQD+0T8nI3kJ2aPLf8xct6wiEPDkRF
734vA43+3iME3SGkkH3Zucz0Xu5zg8OtM78XVW5WmBYZrxCcDnLmu8QJRhhzUI/a
KXdAckHRwyP3Ce69EGCPqw==
-----END PRIVATE KEY-----"""


def generate_jwt(tenant_id: str, role: str, sub: str, extra_claims: dict = None) -> str:
    """Generate a JWT token with the test RSA private key."""
    now = int(time.time())
    payload = {
        "iss": "https://issuer.example",
        "aud": "db9-server",
        "tid": tenant_id,
        "usr": role,
        "sub": sub,
        "exp": now + 3600,
        "iat": now,
    }
    if extra_claims:
        payload.update(extra_claims)
    return pyjwt.encode(
        payload,
        TEST_RSA_PRIVATE_KEY,
        algorithm="RS256",
        headers={"kid": "k1"},
    )


def connect_with_jwt(host: str, port: int, tenant_id: str, role: str, token: str, dbname: str = "postgres"):
    """Connect to db9-server using a JWT token as password."""
    # db9 uses "tenant_id.role" as the username for token auth
    username = f"{tenant_id}.{role}"
    return psycopg2.connect(
        host=host,
        port=port,
        user=username,
        password=token,
        dbname=dbname,
        connect_timeout=10,
    )


def run_tests(host: str, port: int, admin_user: str, admin_password: str, tenant_id: str):
    passed = 0
    failed = 0

    def check(name: str, condition: bool, detail: str = ""):
        nonlocal passed, failed
        if condition:
            print(f"  PASS: {name}")
            passed += 1
        else:
            print(f"  FAIL: {name} {detail}")
            failed += 1

    # --- Setup: connect as admin to create test table and policies ---
    print("Setting up test table and RLS policies...")
    admin_conn = psycopg2.connect(
        host=host, port=port, user=admin_user, password=admin_password, dbname="postgres"
    )
    admin_conn.autocommit = True
    with admin_conn.cursor() as cur:
        cur.execute("DROP TABLE IF EXISTS jwt_rls_test CASCADE")
        cur.execute("DROP ROLE IF EXISTS jwt_app_user")
        cur.execute("CREATE ROLE jwt_app_user LOGIN PASSWORD 'pw'")
        cur.execute("""
            CREATE TABLE jwt_rls_test (
                id INT PRIMARY KEY,
                owner_uid TEXT NOT NULL,
                data TEXT NOT NULL
            )
        """)
        cur.execute("""
            INSERT INTO jwt_rls_test VALUES
                (1, 'user-alice-123', 'alice secret'),
                (2, 'user-bob-456', 'bob secret'),
                (3, 'user-alice-123', 'alice public'),
                (4, 'user-charlie-789', 'charlie data')
        """)
        cur.execute("GRANT SELECT, INSERT ON jwt_rls_test TO jwt_app_user")
        cur.execute("ALTER TABLE jwt_rls_test ENABLE ROW LEVEL SECURITY")
        # Policy: users can only see rows where owner_uid matches their JWT sub claim
        cur.execute("""
            CREATE POLICY jwt_owner_policy ON jwt_rls_test
            FOR ALL TO jwt_app_user
            USING (owner_uid = auth.uid())
            WITH CHECK (owner_uid = auth.uid())
        """)
    admin_conn.close()

    # --- Test 1: auth.uid() returns correct JWT sub claim ---
    print("\nTest 1: auth.uid() returns JWT sub claim")
    token_alice = generate_jwt(tenant_id, "jwt_app_user", "user-alice-123")
    try:
        conn = connect_with_jwt(host, port, tenant_id, "jwt_app_user", token_alice)
        conn.autocommit = True
        with conn.cursor() as cur:
            cur.execute("SELECT auth.uid()")
            result = cur.fetchone()[0]
            check("auth.uid() returns sub claim", result == "user-alice-123",
                  f"expected 'user-alice-123', got '{result}'")
        conn.close()
    except Exception as e:
        check("auth.uid() returns sub claim", False, str(e))

    # --- Test 2: RLS filters rows by JWT identity (alice sees only her rows) ---
    print("\nTest 2: RLS filters rows by JWT identity (alice)")
    try:
        conn = connect_with_jwt(host, port, tenant_id, "jwt_app_user", token_alice)
        conn.autocommit = True
        with conn.cursor() as cur:
            cur.execute("SELECT id, data FROM jwt_rls_test ORDER BY id")
            rows = cur.fetchall()
            check("alice sees exactly 2 rows", len(rows) == 2,
                  f"expected 2 rows, got {len(rows)}: {rows}")
            ids = [r[0] for r in rows]
            check("alice sees rows 1 and 3", ids == [1, 3],
                  f"expected [1, 3], got {ids}")
        conn.close()
    except Exception as e:
        check("RLS filtering for alice", False, str(e))

    # --- Test 3: Different JWT identity sees different rows (bob) ---
    print("\nTest 3: RLS filters rows by JWT identity (bob)")
    token_bob = generate_jwt(tenant_id, "jwt_app_user", "user-bob-456")
    try:
        conn = connect_with_jwt(host, port, tenant_id, "jwt_app_user", token_bob)
        conn.autocommit = True
        with conn.cursor() as cur:
            cur.execute("SELECT id, data FROM jwt_rls_test ORDER BY id")
            rows = cur.fetchall()
            check("bob sees exactly 1 row", len(rows) == 1,
                  f"expected 1 row, got {len(rows)}: {rows}")
            check("bob sees only row 2", rows[0][0] == 2,
                  f"expected id=2, got {rows}")
        conn.close()
    except Exception as e:
        check("RLS filtering for bob", False, str(e))

    # --- Test 4: WITH CHECK prevents inserting rows for other users ---
    print("\nTest 4: WITH CHECK prevents insert for other user's uid")
    try:
        conn = connect_with_jwt(host, port, tenant_id, "jwt_app_user", token_alice)
        conn.autocommit = True
        with conn.cursor() as cur:
            try:
                cur.execute("INSERT INTO jwt_rls_test VALUES (10, 'user-bob-456', 'alice pretends to be bob')")
                check("WITH CHECK blocks cross-user insert", False, "insert should have been rejected")
            except psycopg2.errors.InsufficientPrivilege:
                check("WITH CHECK blocks cross-user insert", True)
            except Exception as e:
                check("WITH CHECK blocks cross-user insert", False, f"unexpected error: {e}")
        conn.close()
    except Exception as e:
        check("WITH CHECK test connection", False, str(e))

    # --- Test 5: auth.uid() in current_setting() ---
    print("\nTest 5: current_setting('auth.uid') matches auth.uid()")
    try:
        conn = connect_with_jwt(host, port, tenant_id, "jwt_app_user", token_alice)
        conn.autocommit = True
        with conn.cursor() as cur:
            cur.execute("SELECT current_setting('auth.uid'), auth.uid()")
            setting_val, func_val = cur.fetchone()
            check("current_setting matches auth.uid()",
                  setting_val == func_val == "user-alice-123",
                  f"setting='{setting_val}', func='{func_val}'")
        conn.close()
    except Exception as e:
        check("current_setting test", False, str(e))

    # --- Test 6: request.jwt.claims contains full JWT payload ---
    print("\nTest 6: request.jwt.claims contains JWT payload")
    try:
        conn = connect_with_jwt(host, port, tenant_id, "jwt_app_user", token_alice)
        conn.autocommit = True
        with conn.cursor() as cur:
            cur.execute("SELECT current_setting('request.jwt.claims')")
            claims_json = cur.fetchone()[0]
            claims = json.loads(claims_json)
            check("JWT claims contain sub", claims.get("sub") == "user-alice-123",
                  f"claims: {claims_json[:200]}")
            check("JWT claims contain iss", claims.get("iss") == "https://issuer.example",
                  f"claims: {claims_json[:200]}")
        conn.close()
    except Exception as e:
        check("JWT claims test", False, str(e))

    # --- Test 7: Anti-spoofing — client cannot SET auth.uid ---
    print("\nTest 7: Anti-spoofing blocks client SET auth.uid")
    try:
        conn = connect_with_jwt(host, port, tenant_id, "jwt_app_user", token_alice)
        conn.autocommit = True
        with conn.cursor() as cur:
            try:
                cur.execute("SET \"auth.uid\" = 'hacker'")
                check("anti-spoofing blocks SET auth.uid", False, "SET should have been rejected")
            except psycopg2.errors.InsufficientPrivilege:
                check("anti-spoofing blocks SET auth.uid", True)
            except Exception as e:
                check("anti-spoofing blocks SET auth.uid", False, f"unexpected error: {e}")
        conn.close()
    except Exception as e:
        check("anti-spoofing test connection", False, str(e))

    # --- Cleanup ---
    print("\nCleaning up...")
    admin_conn = psycopg2.connect(
        host=host, port=port, user=admin_user, password=admin_password, dbname="postgres"
    )
    admin_conn.autocommit = True
    with admin_conn.cursor() as cur:
        cur.execute("DROP TABLE IF EXISTS jwt_rls_test CASCADE")
        cur.execute("DROP ROLE IF EXISTS jwt_app_user")
    admin_conn.close()

    print(f"\nResults: {passed} passed, {failed} failed")
    return failed == 0


def main():
    parser = argparse.ArgumentParser(description="JWT -> auth.uid() -> RLS E2E test")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=5433)
    parser.add_argument("--admin-user", default="root")
    parser.add_argument("--admin-password", default="")
    parser.add_argument("--tenant-id", default="t1",
                        help="Tenant ID for JWT token (must match server keyspace)")
    args = parser.parse_args()

    print("=" * 60)
    print("E2E Test: JWT -> auth.uid() -> RLS Policy Enforcement")
    print("=" * 60)
    print(f"Server: {args.host}:{args.port}")
    print(f"Tenant: {args.tenant_id}")
    print()

    success = run_tests(
        host=args.host,
        port=args.port,
        admin_user=args.admin_user,
        admin_password=args.admin_password,
        tenant_id=args.tenant_id,
    )
    sys.exit(0 if success else 1)


if __name__ == "__main__":
    main()
