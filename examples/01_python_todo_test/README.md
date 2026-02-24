# PostgreSQL TODO App with SQLAlchemy

A demonstration TODO application showcasing PostgreSQL features through SQLAlchemy.

## Features

This application demonstrates the following PostgreSQL features:

1. **UUID Primary Keys** - Using UUID instead of auto-increment integers
2. **JSONB Type** - Storing flexible metadata in JSON format with indexing
3. **ARRAY Type** - Storing tags as native PostgreSQL arrays
4. **Full-Text Search** - Using TSVECTOR and GIN indexes for text search
5. **Enum Types** - Native PostgreSQL enums for priority levels
6. **Automatic Timestamps** - Server-side timestamp generation
7. **GIN Indexes** - Generalized Inverted Indexes for JSONB and arrays
8. **Triggers** - Automatic search vector updates using PL/pgSQL
9. **Check Constraints** - Data validation at the database level
10. **Connection Pooling** - Efficient connection management

## Installation

Using `uv` (recommended):

```bash
# Install dependencies
uv sync

# Or if you want to install in the current environment
uv pip install -e .
```

Using pip (alternative):

```bash
pip install -r requirements.txt
```

## Database Setup

Create a PostgreSQL database:

```bash
createdb todo_db
```

Or using SQL:

```sql
CREATE DATABASE todo_db;
```

## Usage

### Quick Start Script

The easiest way to run the demo:

```bash
./run_demo.sh "postgresql://username:password@localhost:5432/todo_db"
```

### Manual Run

Run the demo with your PostgreSQL connection string:

```bash
# Using uv (recommended)
uv run python run.py "postgresql://username:password@localhost:5432/todo_db"

# Or if dependencies are already installed
python run.py "postgresql://username:password@localhost:5432/todo_db"
```

### Connection String Format

```
postgresql://[user[:password]@][host][:port][/dbname]
```

Examples:
- `postgresql://localhost/todo_db` (local, current user, no password)
- `postgresql://user:pass@localhost:5432/todo_db` (with credentials)
- `postgresql://user:pass@db.example.com/todo_db` (remote)

## Project Structure

```
pg_play/
├── pg_todo/              # Main package
│   ├── __init__.py      # Package initialization
│   ├── models.py        # SQLAlchemy models with PostgreSQL-specific types
│   ├── database.py      # Database connection and session management
│   └── main.py          # Application logic and CRUD operations
├── run.py               # Entry point script
├── pyproject.toml       # Project configuration and dependencies (uv)
├── requirements.txt     # Legacy pip requirements
├── run_demo.sh          # Quick start script
├── .python-version      # Python version specification
├── .env.example         # Environment variable template
├── .gitignore           # Git ignore rules
└── README.md            # This file
```

## PostgreSQL Features Demonstrated

### 1. UUID Primary Keys
```python
id = Column(UUID(as_uuid=True), primary_key=True, default=uuid.uuid4)
```

### 2. JSONB for Metadata
```python
metadata = Column(JSONB, default=dict)
# Query: TodoItem.metadata['key'].astext == 'value'
```

### 3. ARRAY for Tags
```python
tags = Column(ARRAY(String), default=list)
# Query: TodoItem.tags.contains(['tag_name'])
```

### 4. Full-Text Search
```python
search_vector = Column(TSVECTOR)
# Automatically updated via trigger
# Query: TodoItem.search_vector.match('search term')
```

### 5. GIN Indexes
```python
Index('idx_tags_gin', 'tags', postgresql_using='gin')
Index('idx_metadata_gin', 'metadata', postgresql_using='gin')
Index('idx_search_vector', 'search_vector', postgresql_using='gin')
```

### 6. Enum Types
```python
class Priority(enum.Enum):
    LOW = "low"
    MEDIUM = "medium"
    HIGH = "high"
    URGENT = "urgent"

priority = Column(SQLEnum(Priority), default=Priority.MEDIUM)
```

### 7. Triggers (PL/pgSQL)
Automatically updates search_vector when title or description changes:
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

## API Examples

### Create a TODO
```python
app.create_todo(
    title="Learn PostgreSQL",
    description="Study advanced features",
    priority=Priority.HIGH,
    tags=["learning", "database"],
    metadata={"hours": 10, "course": "Advanced SQL"},
    due_date=datetime.now() + timedelta(days=7)
)
```

### Search TODOs (Full-Text)
```python
results = app.search_todos("PostgreSQL")
```

### Find by Tag (ARRAY)
```python
results = app.find_by_tag("learning")
```

### Find by Metadata (JSONB)
```python
results = app.find_by_metadata("course", "Advanced SQL")
```

### Get Statistics
```python
stats = app.get_statistics()
# Returns: total, completed, pending, overdue, by_priority
```

## Environment Variables (Optional)

You can also use a `.env` file:

```env
DATABASE_URL=postgresql://user:password@localhost:5432/todo_db
```

And load it in your code:
```python
from dotenv import load_dotenv
import os

load_dotenv()
dsn = os.getenv('DATABASE_URL')
```

## Testing

The demo function in `main.py` provides a comprehensive test of all features:
- Database connection
- Table creation with triggers
- CRUD operations
- Full-text search
- ARRAY queries
- JSONB queries
- Statistics aggregation

## Compatibility Notes

This application is designed for **standard PostgreSQL**. Some features may not work with PostgreSQL-compatible databases like db9-server (TiKV), CockroachDB, or YugabyteDB.

See [LIMITATIONS.md](LIMITATIONS.md) for detailed compatibility information and workarounds.

### Known Limitations with db9-server
- Full-text search (TSVECTOR) not supported
- ARRAY types return as strings instead of lists
- Cannot drop enum types cleanly
- Some system catalog functions missing

For best results, use **PostgreSQL 12+**.

## License

MIT
