from __future__ import annotations

import os
import sys

import pytest


def main() -> int:
    if not os.environ.get("PG_DSN"):
        print("PG_DSN is required (e.g. postgres://user:pass@127.0.0.1:5433/postgres)", file=sys.stderr)
        return 2
    return pytest.main(["-q", "tests"])


if __name__ == "__main__":
    raise SystemExit(main())
