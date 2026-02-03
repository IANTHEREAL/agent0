"""
Main TODO application with CRUD operations
Demonstrates PostgreSQL features through SQLAlchemy
"""
import sys
from datetime import datetime, timedelta
from sqlalchemy import and_, or_, func, text
from sqlalchemy.dialects.postgresql import insert
from .database import Database
from .models import TodoItem, Priority
import json


class TodoApp:
    """TODO application with CRUD operations"""

    def __init__(self, db: Database):
        self.db = db

    def create_todo(self, title: str, description: str = None,
                   priority: Priority = Priority.MEDIUM,
                   tags: list = None, extra_data: dict = None,
                   due_date: datetime = None) -> TodoItem:
        """
        Create a new TODO item

        Args:
            title: TODO title
            description: Optional description
            priority: Priority level
            tags: List of tags
            extra_data: Additional metadata as JSONB
            due_date: Optional due date

        Returns:
            Created TodoItem
        """
        with self.db.get_session() as session:
            todo = TodoItem(
                title=title,
                description=description,
                priority=priority,
                tags=tags or [],
                extra_data=extra_data or {},
                due_date=due_date
            )
            session.add(todo)
            session.flush()
            session.refresh(todo)
            # Access attributes to load them before session closes
            _ = (todo.id, todo.title, todo.description, todo.priority,
                 todo.tags, todo.extra_data, todo.created_at)
            session.expunge(todo)  # Detach from session but keep loaded attributes
            return todo

    def get_todo(self, todo_id: str) -> TodoItem:
        """Get a TODO item by ID"""
        with self.db.get_session() as session:
            todo = session.query(TodoItem).filter(TodoItem.id == todo_id).first()
            if todo:
                session.expunge(todo)
            return todo

    def update_todo(self, todo_id: str, **kwargs) -> TodoItem:
        """
        Update a TODO item

        Args:
            todo_id: TODO item UUID
            **kwargs: Fields to update

        Returns:
            Updated TodoItem
        """
        with self.db.get_session() as session:
            todo = session.query(TodoItem).filter(TodoItem.id == todo_id).first()
            if not todo:
                raise ValueError(f"Todo with id {todo_id} not found")

            for key, value in kwargs.items():
                if hasattr(todo, key):
                    setattr(todo, key, value)

            session.flush()
            session.refresh(todo)
            session.expunge(todo)
            return todo

    def complete_todo(self, todo_id: str) -> TodoItem:
        """Mark a TODO as completed"""
        return self.update_todo(
            todo_id,
            is_completed=True,
            completed_at=datetime.now()
        )

    def delete_todo(self, todo_id: str) -> bool:
        """Delete a TODO item"""
        with self.db.get_session() as session:
            todo = session.query(TodoItem).filter(TodoItem.id == todo_id).first()
            if todo:
                session.delete(todo)
                return True
            return False

    def list_todos(self, completed: bool = None, priority: Priority = None,
                  limit: int = 100, offset: int = 0) -> list[TodoItem]:
        """
        List TODO items with filters

        Args:
            completed: Filter by completion status
            priority: Filter by priority
            limit: Maximum number of results
            offset: Number of results to skip

        Returns:
            List of TodoItem objects
        """
        with self.db.get_session() as session:
            query = session.query(TodoItem)

            if completed is not None:
                query = query.filter(TodoItem.is_completed == completed)

            if priority is not None:
                query = query.filter(TodoItem.priority == priority)

            query = query.order_by(TodoItem.created_at.desc())
            query = query.limit(limit).offset(offset)

            todos = query.all()
            for todo in todos:
                session.expunge(todo)
            return todos

    def search_todos(self, search_text: str) -> list[TodoItem]:
        """
        Full-text search on TODO items (PostgreSQL feature)

        Args:
            search_text: Text to search for

        Returns:
            List of matching TodoItem objects
        """
        with self.db.get_session() as session:
            # Use PostgreSQL full-text search
            query = session.query(TodoItem).filter(
                TodoItem.search_vector.match(search_text)
            ).order_by(
                func.ts_rank(TodoItem.search_vector, func.to_tsquery('english', search_text)).desc()
            )
            todos = query.all()
            for todo in todos:
                session.expunge(todo)
            return todos

    def find_by_tag(self, tag: str) -> list[TodoItem]:
        """
        Find TODOs by tag using ARRAY contains (PostgreSQL feature)

        Args:
            tag: Tag to search for

        Returns:
            List of TodoItem objects with the tag
        """
        with self.db.get_session() as session:
            # PostgreSQL array contains operator
            query = session.query(TodoItem).filter(
                TodoItem.tags.contains([tag])
            )
            todos = query.all()
            for todo in todos:
                session.expunge(todo)
            return todos

    def find_by_metadata(self, key: str, value) -> list[TodoItem]:
        """
        Find TODOs by extra_data using JSONB queries (PostgreSQL feature)

        Args:
            key: Metadata key
            value: Expected value

        Returns:
            List of matching TodoItem objects
        """
        with self.db.get_session() as session:
            # PostgreSQL JSONB query
            query = session.query(TodoItem).filter(
                TodoItem.extra_data[key].astext == str(value)
            )
            todos = query.all()
            for todo in todos:
                session.expunge(todo)
            return todos

    def get_overdue_todos(self) -> list[TodoItem]:
        """Get incomplete TODOs that are past their due date"""
        with self.db.get_session() as session:
            now = datetime.now()
            query = session.query(TodoItem).filter(
                and_(
                    TodoItem.is_completed == False,
                    TodoItem.due_date < now
                )
            ).order_by(TodoItem.due_date)
            todos = query.all()
            for todo in todos:
                session.expunge(todo)
            return todos

    def upsert_todo(self, title: str, **kwargs) -> TodoItem:
        """
        Insert or update TODO using PostgreSQL UPSERT (ON CONFLICT)

        Args:
            title: TODO title (used for conflict detection)
            **kwargs: Other fields

        Returns:
            TodoItem
        """
        with self.db.get_session() as session:
            # PostgreSQL INSERT ... ON CONFLICT DO UPDATE
            stmt = insert(TodoItem).values(
                title=title,
                **kwargs
            ).on_conflict_do_update(
                index_elements=['title'],
                set_=kwargs
            ).returning(TodoItem)

            result = session.execute(stmt)
            return result.scalar_one()

    def get_statistics(self) -> dict:
        """
        Get TODO statistics using PostgreSQL aggregate functions

        Returns:
            Dictionary with statistics
        """
        with self.db.get_session() as session:
            # Count by completion status
            total = session.query(func.count(TodoItem.id)).scalar()
            completed = session.query(func.count(TodoItem.id)).filter(
                TodoItem.is_completed == True
            ).scalar()

            # Count by priority
            priority_counts = session.query(
                TodoItem.priority,
                func.count(TodoItem.id)
            ).group_by(TodoItem.priority).all()

            # Get overdue count
            overdue = session.query(func.count(TodoItem.id)).filter(
                and_(
                    TodoItem.is_completed == False,
                    TodoItem.due_date < datetime.now()
                )
            ).scalar()

            return {
                'total': total,
                'completed': completed,
                'pending': total - completed,
                'overdue': overdue,
                'by_priority': {str(p): c for p, c in priority_counts}
            }


def demo(dsn: str):
    """
    Demonstration of the TODO app with PostgreSQL features

    Args:
        dsn: PostgreSQL connection string
    """
    print("=" * 60)
    print("TODO App - PostgreSQL Features Demo")
    print("=" * 60)

    # Initialize database
    db = Database(dsn)

    # Test connection
    print("\n1. Testing database connection...")
    if not db.test_connection():
        print("Failed to connect to database!")
        return

    # Create tables
    print("\n2. Creating tables and triggers...")
    try:
        db.drop_tables()  # Clean slate - may fail on non-standard PostgreSQL
    except Exception as e:
        print(f"   Note: Could not drop existing tables (this is OK): {str(e)[:100]}")

    try:
        db.create_tables()
    except Exception as e:
        print(f"   Note: Tables may already exist (this is OK): {str(e)[:100]}")

    # Initialize app
    app = TodoApp(db)

    # Create some TODO items
    print("\n3. Creating TODO items...")
    todo1 = app.create_todo(
        title="Learn PostgreSQL",
        description="Study advanced PostgreSQL features",
        priority=Priority.HIGH,
        tags=["learning", "database"],
        extra_data={"course": "Advanced SQL", "hours": 10},
        due_date=datetime.now() + timedelta(days=7)
    )
    print(f"   Created: {todo1.title} (ID: {todo1.id})")

    todo2 = app.create_todo(
        title="Build TODO app",
        description="Create a TODO application with SQLAlchemy",
        priority=Priority.URGENT,
        tags=["project", "python"],
        extra_data={"repository": "github.com/user/todo", "language": "Python"}
    )
    print(f"   Created: {todo2.title} (ID: {todo2.id})")

    todo3 = app.create_todo(
        title="Write documentation",
        description="Document all PostgreSQL features used",
        priority=Priority.MEDIUM,
        tags=["documentation", "writing"],
        due_date=datetime.now() + timedelta(days=3)
    )
    print(f"   Created: {todo3.title} (ID: {todo3.id})")

    # List all TODOs
    print("\n4. Listing all TODOs...")
    all_todos = app.list_todos()
    for todo in all_todos:
        tags_display = todo.tags if isinstance(todo.tags, list) else "N/A"
        print(f"   - {todo.title} [{todo.priority.value}] - Tags: {tags_display}")

    # Full-text search (PostgreSQL feature)
    print("\n5. Full-text search for 'PostgreSQL'...")
    try:
        search_results = app.search_todos("PostgreSQL")
        for todo in search_results:
            print(f"   - {todo.title}")
    except Exception as e:
        print(f"   Note: Full-text search not supported on this database: {str(e)[:80]}")

    # Find by tag (ARRAY feature)
    print("\n6. Finding TODOs with tag 'learning'...")
    try:
        tagged_todos = app.find_by_tag("learning")
        for todo in tagged_todos:
            tags_display = todo.tags if isinstance(todo.tags, list) else "N/A"
            print(f"   - {todo.title} - Tags: {tags_display}")
    except Exception as e:
        print(f"   Note: ARRAY search not supported on this database: {str(e)[:80]}")

    # Find by extra_data (JSONB feature)
    print("\n7. Finding TODOs with extra_data language='Python'...")
    try:
        python_todos = app.find_by_metadata("language", "Python")
        for todo in python_todos:
            print(f"   - {todo.title} - Extra Data: {todo.extra_data}")
    except Exception as e:
        print(f"   Note: JSONB search not fully supported on this database: {str(e)[:80]}")

    # Complete a TODO
    print("\n8. Completing a TODO...")
    completed = app.complete_todo(str(todo2.id))
    print(f"   Completed: {completed.title} at {completed.completed_at}")

    # Get statistics
    print("\n9. Getting statistics...")
    stats = app.get_statistics()
    print(f"   Total: {stats['total']}")
    print(f"   Completed: {stats['completed']}")
    print(f"   Pending: {stats['pending']}")
    print(f"   Overdue: {stats['overdue']}")
    print(f"   By Priority: {json.dumps(stats['by_priority'], indent=6)}")

    # Get overdue TODOs
    print("\n10. Checking overdue TODOs...")
    overdue = app.get_overdue_todos()
    if overdue:
        for todo in overdue:
            print(f"   - {todo.title} (Due: {todo.due_date})")
    else:
        print("   No overdue TODOs!")

    print("\n" + "=" * 60)
    print("Demo completed!")
    print("=" * 60)


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print("Usage: python main.py <postgresql_dsn>")
        print("Example: python main.py postgresql://user:password@localhost:5432/todo_db")
        sys.exit(1)

    dsn = sys.argv[1]
    demo(dsn)
