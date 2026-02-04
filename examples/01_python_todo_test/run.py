"""
Entry point for running the TODO app demo
"""
import sys
from pg_todo.main import demo

if __name__ == "__main__":
    if len(sys.argv) < 2:
        print("Usage: python run.py <postgresql_dsn>")
        print("Example: python run.py postgresql://user:password@localhost:5432/todo_db")
        sys.exit(1)

    dsn = sys.argv[1]
    demo(dsn)
