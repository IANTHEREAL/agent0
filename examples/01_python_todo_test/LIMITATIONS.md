# Limitations and Issues Encountered

This document records all limitations, issues, and workarounds discovered during the development and testing of this PostgreSQL TODO app with SQLAlchemy.

## Environment

- **Database**: PostgreSQL 16.0 (pg-tikv 0.1.0 on TiKV)
- **SQLAlchemy**: 2.0.25
- **psycopg2**: 2.9.9
- **Python**: 3.10.18

---

## 1. SQLAlchemy Reserved Attribute Names

### Issue
```python
# This causes an error:
class TodoItem(Base):
    metadata = Column(JSONB, default=dict)  # ❌ FAILS
```

### Error
```
sqlalchemy.exc.InvalidRequestError: Attribute name 'metadata' is reserved
when using the Declarative API.
```

### Root Cause
SQLAlchemy's declarative `Base` class already has a `metadata` attribute that holds the schema metadata (tables, columns, etc.). Using `metadata` as a column name creates a conflict.

### Solution
Rename the field to something else:
```python
class TodoItem(Base):
    extra_data = Column(JSONB, default=dict)  # ✅ WORKS
```

### Reference
- File: `pg_todo/models.py:49`

---

## 2. DSN Format Compatibility (postgres:// vs postgresql://)

### Issue
SQLAlchemy 2.0+ requires `postgresql://` scheme, but many tools/services provide `postgres://` DSNs.

### Error
```
sqlalchemy.exc.NoSuchModuleError: Can't load plugin: sqlalchemy.dialects:postgres
```

### Root Cause
SQLAlchemy 2.0 deprecated the `postgres://` scheme in favor of `postgresql://` for consistency.

### Solution
Auto-convert in the Database class:
```python
def __init__(self, dsn: str):
    # SQLAlchemy 2.0+ requires 'postgresql://' not 'postgres://'
    if dsn.startswith('postgres://'):
        dsn = dsn.replace('postgres://', 'postgresql://', 1)
    self.dsn = dsn
```

### Reference
- File: `pg_todo/database.py:23-25`
- Documentation: https://docs.sqlalchemy.org/en/20/core/engines.html#postgresql

---

## 3. Session Management and Detached Instances

### Issue
Accessing ORM object attributes outside the session context causes errors:

```python
with db.get_session() as session:
    todo = TodoItem(...)
    session.add(todo)
    # Session ends here

print(todo.id)  # ❌ FAILS - DetachedInstanceError
```

### Error
```
sqlalchemy.orm.exc.DetachedInstanceError: Instance <TodoItem at 0x...> is not
bound to a Session; attribute refresh operation cannot proceed
```

### Root Cause
When the session closes, ORM objects become "detached". SQLAlchemy can't lazy-load attributes anymore.

### Solution
Explicitly detach objects with loaded attributes before returning them:

```python
def create_todo(self, ...) -> TodoItem:
    with self.db.get_session() as session:
        todo = TodoItem(...)
        session.add(todo)
        session.flush()
        session.refresh(todo)
        # Access attributes to force loading
        _ = (todo.id, todo.title, todo.description, todo.priority,
             todo.tags, todo.extra_data, todo.created_at)
        session.expunge(todo)  # Detach but keep loaded data
        return todo
```

### Reference
- File: `pg_todo/main.py:32-34`
- Documentation: https://docs.sqlalchemy.org/en/20/orm/session_state_management.html

---

## 4. pg-tikv: Missing pg_type_is_visible Function

### Issue
Dropping or checking enum types fails on pg-tikv.

### Error
```
psycopg2.errors.InternalError_: Unsupported function: pg_type_is_visible

[SQL: SELECT pg_catalog.pg_type.typname
FROM pg_catalog.pg_type JOIN pg_catalog.pg_namespace ...
WHERE ... AND pg_catalog.pg_type_is_visible(pg_catalog.pg_type.oid) ...]
```

### Root Cause
pg-tikv (PostgreSQL on TiKV) doesn't implement all PostgreSQL system catalog functions. The `pg_type_is_visible()` function is used by SQLAlchemy to check if enum types exist before creating/dropping them.

### Impact
- Cannot drop tables with enum types cleanly
- Cannot use `checkfirst=True` when creating enum types
- `Base.metadata.drop_all()` fails

### Workaround
Wrap drop operations in try-except:
```python
try:
    db.drop_tables()
except Exception as e:
    print(f"Note: Could not drop tables: {e}")
```

### Reference
- File: `pg_todo/main.py:298-303`
- Related: pg-tikv doesn't fully support PostgreSQL system catalogs

---

## 5. pg-tikv: Full-Text Search Not Supported

### Issue
TSVECTOR and full-text search operators don't work on pg-tikv.

### Error
```
psycopg2.errors.InternalError_: Filter predicate must evaluate to boolean

[SQL: SELECT ... FROM todo_items
WHERE todo_items.search_vector @@ plainto_tsquery(%(search_vector_1)s)
ORDER BY ts_rank(todo_items.search_vector, to_tsquery(...)) DESC]
```

### Root Cause
pg-tikv doesn't support:
- `TSVECTOR` type (stored as text but not functional)
- `@@` match operator
- `ts_rank()` function
- `to_tsquery()`, `plainto_tsquery()` functions
- Full-text search triggers

### Impact
- Cannot use PostgreSQL full-text search features
- Triggers using `to_tsvector()` are created but don't function
- Search queries fail at runtime

### Workaround
Detect and skip full-text search in demo:
```python
try:
    search_results = app.search_todos("PostgreSQL")
except Exception as e:
    print(f"Note: Full-text search not supported: {e}")
```

### Alternative Solutions
- Use `LIKE` or `ILIKE` for simple text search
- Use external search engine (Elasticsearch, MeiliSearch)
- Implement application-level search

### Reference
- File: `pg_todo/main.py:342-347`

---

## 6. pg-tikv: ARRAY Type Returns String Instead of List

### Issue
PostgreSQL ARRAY columns are returned as strings instead of Python lists.

### Expected Behavior
```python
todo.tags  # Should be: ['learning', 'database']
```

### Actual Behavior
```python
todo.tags  # Returns: ['{', 'l', 'e', 'a', 'r', 'n', 'i', 'n', 'g', ',', ...]
# String "{learning,database}" treated as Python string, not parsed as array
```

### Root Cause
pg-tikv stores arrays as text representations but doesn't properly convert them back to Python lists when retrieving data. The psycopg2 driver expects PostgreSQL to provide proper array type information, but pg-tikv doesn't.

### Impact
- Array data is stored correctly
- Array queries (`contains`) work for finding data
- Display/iteration over arrays doesn't work as expected

### Workaround
Check type before displaying:
```python
tags_display = todo.tags if isinstance(todo.tags, list) else "N/A"
print(f"Tags: {tags_display}")
```

### Proper Solution (for production)
Create a custom type decorator:
```python
from sqlalchemy.types import TypeDecorator, String
import json

class PgTikvArray(TypeDecorator):
    impl = String
    cache_ok = True

    def process_result_value(self, value, dialect):
        if value is None:
            return []
        if isinstance(value, list):
            return value
        # Parse PostgreSQL array format: {item1,item2}
        if value.startswith('{') and value.endswith('}'):
            items = value[1:-1].split(',')
            return [item.strip() for item in items if item]
        return []

    def process_bind_param(self, value, dialect):
        if value is None or len(value) == 0:
            return None
        return value

# Then use:
tags = Column(PgTikvArray, default=list)  # For pg-tikv
# OR
tags = Column(ARRAY(String), default=list)  # For standard PostgreSQL
```

### Reference
- File: `pg_todo/main.py:338` (display workaround)

---

## 7. pg-tikv: GIN Indexes Created But May Not Function

### Issue
GIN indexes for JSONB and ARRAY are created without error, but their effectiveness is uncertain.

### Code
```python
Index('idx_tags_gin', 'tags', postgresql_using='gin'),
Index('idx_extra_data_gin', 'extra_data', postgresql_using='gin'),
Index('idx_search_vector', 'search_vector', postgresql_using='gin'),
```

### Observation
- Index creation succeeds
- JSONB queries work correctly
- ARRAY queries work but return string data
- Performance characteristics unknown

### Impact
May not provide the expected performance benefits of GIN indexes.

### Reference
- File: `pg_todo/models.py:75-77`

---

## 8. pg-tikv: Trigger Functions Created But Don't Execute

### Issue
PL/pgSQL trigger functions are created successfully but don't update the `search_vector` column.

### Code
```sql
CREATE OR REPLACE FUNCTION update_search_vector()
RETURNS trigger AS $$
BEGIN
    NEW.search_vector :=
        setweight(to_tsvector('english', COALESCE(NEW.title, '')), 'A') ||
        setweight(to_tsvector('english', COALESCE(NEW.description, '')), 'B');
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
```

### Observation
- Function creation: ✅ Succeeds
- Trigger creation: ✅ Succeeds
- Trigger execution: ❌ Fails silently (search_vector stays NULL)

### Root Cause
pg-tikv doesn't support `to_tsvector()` function, so the trigger fails when executed.

### Reference
- File: `pg_todo/database.py:64-81`

---

## 9. Connection String Format Confusion

### Issue
Username and password encoding in DSN can be confusing.

### Wrong Format
```bash
postgres://username.password@host:port/db  # ❌ Treats "username.password" as username
```

### Correct Format
```bash
postgres://username:password@host:port/db  # ✅ Colon separates user and password
```

### Special Characters
If username or password contains special characters, URL-encode them:
```python
from urllib.parse import quote_plus

username = "user@domain"
password = "p@ss:word"
dsn = f"postgresql://{quote_plus(username)}:{quote_plus(password)}@host/db"
```

---

## 10. uv Build System Requirements

### Issue
Using `uv` with editable install requires proper package structure.

### Error
```
ValueError: Unable to determine which files to ship inside the wheel using
the following heuristics: https://hatch.pypa.io/latest/plugins/builder/wheel/#default-file-selection

The most likely cause of this is that there is no directory that matches the
name of your project (pg_todo).
```

### Root Cause
Hatchling build backend expects:
- Python files in a package directory matching project name
- OR explicit file selection in `pyproject.toml`

### Solution
Create proper package structure:
```
pg_play/
├── pg_todo/           # Package directory (matches project name)
│   ├── __init__.py
│   ├── models.py
│   ├── database.py
│   └── main.py
├── run.py             # Entry point
└── pyproject.toml
```

### Reference
- Package structure created to resolve this issue

---

## Summary: pg-tikv vs Standard PostgreSQL

### Supported Features ✅
- UUID type
- JSONB type and queries
- ARRAY type (with limitations)
- Enum types (can't be dropped cleanly)
- Timestamps and timezone support
- Aggregate functions
- Transactions
- Connection pooling
- Basic indexes (B-tree)
- Check constraints

### Unsupported Features ❌
- Full-text search (TSVECTOR, @@, ts_rank, etc.)
- Proper ARRAY type conversion (returns strings)
- System catalog functions (pg_type_is_visible, etc.)
- Trigger functions using unsupported features
- GIN indexes (created but may not work)

### Recommendation
**For learning/demonstration of PostgreSQL features**: Use standard PostgreSQL

**For production with pg-tikv**: Avoid advanced PostgreSQL-specific features, use common SQL features instead

---

## Testing with Standard PostgreSQL

To test with full feature support, use standard PostgreSQL:

```bash
# Using Docker
docker run --name postgres-test -e POSTGRES_PASSWORD=password -p 5432:5432 -d postgres:16

# Create database
docker exec -it postgres-test createdb -U postgres todo_db

# Run demo
uv run python run.py "postgresql://postgres:password@localhost:5432/todo_db"
```

All features including full-text search, proper ARRAY handling, and enum management will work correctly with standard PostgreSQL.
