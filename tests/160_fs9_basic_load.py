#!/usr/bin/env python3
"""Create filesystem fixtures for fs9 integration tests."""

import argparse
import os
import shutil


FIXTURE_DIR = "/tmp/pgtikv-fs9-test"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--user", required=True)
    parser.add_argument("--password", required=True)
    parser.parse_args()

    if os.path.exists(FIXTURE_DIR):
        shutil.rmtree(FIXTURE_DIR)

    os.makedirs(FIXTURE_DIR, exist_ok=True)

    with open(os.path.join(FIXTURE_DIR, "hello.txt"), "w", encoding="utf-8") as f:
        f.write("Hello World\nThis is line two\nThird line here\n")

    with open(os.path.join(FIXTURE_DIR, "users.csv"), "w", encoding="utf-8") as f:
        f.write("name,age,city\nAlice,30,Beijing\nBob,25,Shanghai\nCharlie,35,Shenzhen\n")

    with open(os.path.join(FIXTURE_DIR, "data.tsv"), "w", encoding="utf-8") as f:
        f.write("id\tvalue\n1\talpha\n2\tbeta\n")

    with open(os.path.join(FIXTURE_DIR, "logs.jsonl"), "w", encoding="utf-8") as f:
        f.write('{"level":"INFO","message":"started"}\n')
        f.write('{"level":"WARN","message":"slow query"}\n')
        f.write('{"level":"ERROR","message":"connection lost"}\n')

    with open(os.path.join(FIXTURE_DIR, "empty.csv"), "w", encoding="utf-8"):
        pass

    with open(os.path.join(FIXTURE_DIR, "header_only.csv"), "w", encoding="utf-8") as f:
        f.write("name,age,city\n")

    subdir = os.path.join(FIXTURE_DIR, "subdir")
    os.makedirs(subdir, exist_ok=True)
    with open(os.path.join(subdir, "nested.txt"), "w", encoding="utf-8") as f:
        f.write("nested content\n")

    sales_dir = os.path.join(FIXTURE_DIR, "sales")
    os.makedirs(sales_dir, exist_ok=True)
    with open(os.path.join(sales_dir, "jan.csv"), "w", encoding="utf-8") as f:
        f.write("product,amount\nWidget,100\nGadget,200\n")
    with open(os.path.join(sales_dir, "feb.csv"), "w", encoding="utf-8") as f:
        f.write("product,amount\nWidget,150\nGadget,250\n")

    print(f"Fixtures created at {FIXTURE_DIR}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
