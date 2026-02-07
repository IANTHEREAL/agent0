#!/usr/bin/env python3
"""pgtikv-ctl — CLI for pg-tikv Admin Portal API."""

import argparse
import json
import os
import sys
from datetime import datetime
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

DEFAULT_API_URL = "http://localhost:8090/api"


def _api(method: str, path: str, *, api_url: str, api_key: str | None = None, body: dict | None = None) -> dict | list | None:
    url = f"{api_url.rstrip('/')}{path}"
    headers = {"Content-Type": "application/json"}
    if api_key:
        headers["X-API-Key"] = api_key

    data = json.dumps(body).encode() if body else None
    req = Request(url, data=data, headers=headers, method=method)

    try:
        with urlopen(req, timeout=30) as resp:
            if resp.status == 204:
                return None
            return json.loads(resp.read())
    except HTTPError as e:
        err = json.loads(e.read()) if e.fp else {}
        detail = err.get("detail", err.get("message", e.reason))
        print(f"Error {e.code}: {detail}", file=sys.stderr)
        sys.exit(1)
    except URLError as e:
        print(f"Connection failed: {e.reason}", file=sys.stderr)
        sys.exit(1)


def _print_table(rows: list[dict], columns: list[tuple[str, str, int]]):
    if not rows:
        print("(empty)")
        return

    widths = []
    for header, key, min_w in columns:
        max_val = max((len(str(r.get(key, ""))) for r in rows), default=0)
        widths.append(max(len(header), max_val, min_w))

    header_line = "  ".join(h.ljust(w) for (h, _, _), w in zip(columns, widths))
    print(header_line)
    print("  ".join("─" * w for w in widths))

    for row in rows:
        vals = []
        for (_, key, _), w in zip(columns, widths):
            v = row.get(key, "")
            if isinstance(v, list):
                v = ", ".join(str(x) for x in v) if v else "-"
            elif v is None:
                v = "-"
            vals.append(str(v).ljust(w))
        print("  ".join(vals))


def _print_json(data):
    print(json.dumps(data, indent=2, default=str))


def _format_time(iso_str: str | None) -> str:
    if not iso_str:
        return "-"
    try:
        dt = datetime.fromisoformat(iso_str.replace("Z", "+00:00"))
        return dt.strftime("%Y-%m-%d %H:%M")
    except (ValueError, AttributeError):
        return iso_str


# ── Tenant commands ──────────────────────────────────────────────

def cmd_tenant_list(args):
    params = [f"page={args.page}", f"size={args.size}"]
    if args.state:
        params.append(f"state={args.state}")
    if args.query:
        params.append(f"q={args.query}")

    data = _api("GET", f"/tenants?{'&'.join(params)}", api_url=args.api_url, api_key=args.api_key)

    if args.json:
        _print_json(data)
        return

    items = data.get("items", [])
    total = data.get("total", 0)

    for t in items:
        t["created_at"] = _format_time(t.get("created_at"))

    _print_table(items, [
        ("ID", "id", 12),
        ("STATE", "state", 8),
        ("CREATED", "created_at", 16),
        ("TAGS", "tags", 10),
    ])
    print(f"\n{total} tenant(s), page {data.get('page')}/{max(1, -(-total // args.size))}")


def cmd_tenant_get(args):
    data = _api("GET", f"/tenants/{args.tenant_id}", api_url=args.api_url, api_key=args.api_key)

    if args.json:
        _print_json(data)
        return

    print(f"Tenant:       t{data['id']}")
    print(f"State:        {data.get('state', '-')}")
    print(f"Created:      {_format_time(data.get('created_at'))}")
    if data.get("state_reason"):
        print(f"State Reason: {data['state_reason']}")
    if data.get("notes"):
        print(f"Notes:        {data['notes']}")
    if data.get("tags"):
        print(f"Tags:         {', '.join(data['tags'])}")

    endpoints = data.get("endpoints", [])
    if endpoints:
        print(f"\nEndpoints:")
        for ep in endpoints:
            print(f"  {ep['host']}:{ep['port']}  ({ep.get('type', '-')}, priority={ep.get('priority', '-')})")


def cmd_tenant_create(args):
    body = {"admin_user": args.admin_user}
    if args.admin_password:
        body["admin_password"] = args.admin_password

    data = _api("POST", "/tenants", api_url=args.api_url, api_key=args.api_key, body=body)

    if args.json:
        _print_json(data)
        return

    print(f"Tenant created: t{data['id']}")
    print(f"Admin user:     {data['admin_user']}")
    print(f"Admin password: {data['admin_password']}")
    print(f"Connection:     {data['connection_string']}")


def cmd_tenant_remove(args):
    data = _api("POST", f"/tenants/{args.tenant_id}/remove", api_url=args.api_url, api_key=args.api_key)
    print(data.get("message", "Done"))


def cmd_tenant_delete(args):
    data = _api("DELETE", f"/tenants/{args.tenant_id}", api_url=args.api_url, api_key=args.api_key)
    print(data.get("message", "Done"))


def cmd_tenant_update(args):
    body = {}
    if args.notes is not None:
        body["notes"] = args.notes if args.notes != "" else None
    if args.tags is not None:
        body["tags"] = [t.strip() for t in args.tags.split(",") if t.strip()] if args.tags else None

    data = _api("PUT", f"/tenants/{args.tenant_id}", api_url=args.api_url, api_key=args.api_key, body=body)

    if args.json:
        _print_json(data)
        return

    print(f"Updated t{data['id']} (state={data.get('state')})")


# ── Connect ──────────────────────────────────────────────────────

def cmd_connect(args):
    body = {"admin_user": args.admin_user, "admin_password": args.admin_password}
    data = _api("POST", f"/tenants/{args.tenant_id}/connect", api_url=args.api_url, api_key=args.api_key, body=body)

    if args.json:
        _print_json(data)
        return

    print(f"Session:  {data['session_id']}")
    print(f"Expires:  {_format_time(data.get('expires_at'))}")
    print(f"\nUse with: pgtikv-ctl users list {args.tenant_id} --session {data['session_id']}")


# ── User commands ────────────────────────────────────────────────

def _user_headers(args) -> dict:
    if not args.session:
        print("Error: --session required. Use 'pgtikv-ctl connect <tenant_id>' first.", file=sys.stderr)
        sys.exit(1)
    return {"X-Tenant-Session": args.session}


def _api_with_session(method, path, args, body=None):
    from urllib.request import Request as Req, urlopen as uopen
    url = f"{args.api_url.rstrip('/')}{path}"
    headers = {"Content-Type": "application/json"}
    if args.api_key:
        headers["X-API-Key"] = args.api_key
    if args.session:
        headers["X-Tenant-Session"] = args.session

    data = json.dumps(body).encode() if body else None
    req = Req(url, data=data, headers=headers, method=method)

    try:
        with uopen(req, timeout=30) as resp:
            if resp.status == 204:
                return None
            return json.loads(resp.read())
    except HTTPError as e:
        err = json.loads(e.read()) if e.fp else {}
        detail = err.get("detail", err.get("message", e.reason))
        print(f"Error {e.code}: {detail}", file=sys.stderr)
        sys.exit(1)
    except URLError as e:
        print(f"Connection failed: {e.reason}", file=sys.stderr)
        sys.exit(1)


def cmd_user_list(args):
    data = _api_with_session("GET", f"/tenants/{args.tenant_id}/users", args)

    if args.json:
        _print_json(data)
        return

    _print_table(data, [
        ("NAME", "name", 15),
        ("SUPERUSER", "is_superuser", 9),
        ("LOGIN", "can_login", 5),
    ])


def cmd_user_create(args):
    body = {"username": args.username}
    if args.password:
        body["password"] = args.password
    if args.superuser:
        body["superuser"] = True

    data = _api_with_session("POST", f"/tenants/{args.tenant_id}/users", args, body=body)

    if args.json:
        _print_json(data)
        return

    print(f"User created: {data['username']}")
    print(f"Password:     {data['password']}")
    print(f"Connect:      {data.get('connection', '-')}")


def cmd_user_delete(args):
    data = _api_with_session("DELETE", f"/tenants/{args.tenant_id}/users/{args.username}", args)
    print(data.get("message", "Done"))


def cmd_user_reset_password(args):
    data = _api_with_session("POST", f"/tenants/{args.tenant_id}/users/{args.username}/password", args)

    if args.json:
        _print_json(data)
        return

    print(f"Password reset for {data['username']}: {data['password']}")


# ── System commands ──────────────────────────────────────────────

def cmd_health(args):
    data = _api("GET", "/health", api_url=args.api_url, api_key=args.api_key)

    if args.json:
        _print_json(data)
        return

    status = data.get("status", "unknown")
    pd = "✓" if data.get("pd_healthy") else "✗"
    print(f"Status: {status}  PD: {pd}")


def cmd_info(args):
    data = _api("GET", "/info", api_url=args.api_url, api_key=args.api_key)
    if args.json:
        _print_json(data)
    else:
        print(f"{data.get('name', 'pg-tikv Admin API')} v{data.get('version', '?')}")


# ── Parser ───────────────────────────────────────────────────────

def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="pgtikv-ctl",
        description="pg-tikv Admin Portal CLI",
    )
    p.add_argument("--api-url", default=os.environ.get("PGTIKV_API_URL", DEFAULT_API_URL),
                   help=f"API base URL (env: PGTIKV_API_URL, default: {DEFAULT_API_URL})")
    p.add_argument("--api-key", default=os.environ.get("PGTIKV_API_KEY"),
                   help="API key (env: PGTIKV_API_KEY)")
    p.add_argument("--json", action="store_true", help="Output as JSON")

    sub = p.add_subparsers(dest="command", required=True)

    # ── tenants ──
    t_sub = sub.add_parser("tenants", help="Tenant management").add_subparsers(dest="subcommand", required=True)

    ls = t_sub.add_parser("list", help="List tenants")
    ls.add_argument("--page", type=int, default=1)
    ls.add_argument("--size", type=int, default=50)
    ls.add_argument("--state", help="Filter by state (ACTIVE, CREATING, DISABLED, ...)")
    ls.add_argument("-q", "--query", help="Search by tenant ID")
    ls.set_defaults(func=cmd_tenant_list)

    get = t_sub.add_parser("get", help="Get tenant details")
    get.add_argument("tenant_id")
    get.set_defaults(func=cmd_tenant_get)

    create = t_sub.add_parser("create", help="Create tenant")
    create.add_argument("--admin-user", default="admin")
    create.add_argument("--admin-password", help="Auto-generated if omitted")
    create.set_defaults(func=cmd_tenant_create)

    rm = t_sub.add_parser("remove", help="Remove tenant (ACTIVE → DISABLED)")
    rm.add_argument("tenant_id")
    rm.set_defaults(func=cmd_tenant_remove)

    dl = t_sub.add_parser("delete", help="Delete tenant (alias for remove)")
    dl.add_argument("tenant_id")
    dl.set_defaults(func=cmd_tenant_delete)

    upd = t_sub.add_parser("update", help="Update tenant metadata")
    upd.add_argument("tenant_id")
    upd.add_argument("--notes", help="Set notes (empty string to clear)")
    upd.add_argument("--tags", help="Comma-separated tags (empty to clear)")
    upd.set_defaults(func=cmd_tenant_update)

    # ── connect ──
    conn = sub.add_parser("connect", help="Get tenant session for user management")
    conn.add_argument("tenant_id")
    conn.add_argument("--admin-user", required=True)
    conn.add_argument("--admin-password", required=True)
    conn.set_defaults(func=cmd_connect)

    # ── users ──
    u_sub = sub.add_parser("users", help="User management (requires --session)").add_subparsers(dest="subcommand", required=True)

    def _add_session(parser):
        parser.add_argument("--session", required=True, help="Tenant session ID from 'connect'")

    uls = u_sub.add_parser("list", help="List users")
    uls.add_argument("tenant_id")
    _add_session(uls)
    uls.set_defaults(func=cmd_user_list)

    ucr = u_sub.add_parser("create", help="Create user")
    ucr.add_argument("tenant_id")
    ucr.add_argument("--username", required=True)
    ucr.add_argument("--password", help="Auto-generated if omitted")
    ucr.add_argument("--superuser", action="store_true")
    _add_session(ucr)
    ucr.set_defaults(func=cmd_user_create)

    udl = u_sub.add_parser("delete", help="Delete user")
    udl.add_argument("tenant_id")
    udl.add_argument("username")
    _add_session(udl)
    udl.set_defaults(func=cmd_user_delete)

    urst = u_sub.add_parser("reset-password", help="Reset user password")
    urst.add_argument("tenant_id")
    urst.add_argument("username")
    _add_session(urst)
    urst.set_defaults(func=cmd_user_reset_password)

    # ── system ──
    h = sub.add_parser("health", help="Health check")
    h.set_defaults(func=cmd_health)

    i = sub.add_parser("info", help="API info")
    i.set_defaults(func=cmd_info)

    return p


def main():
    parser = build_parser()
    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
