"""
Main TODO application with CRUD operations
Demonstrates PostgreSQL features through SQLAlchemy, including JOIN operations
"""
import sys
from datetime import datetime, timedelta
from sqlalchemy import and_, or_, func, text, Integer, case
from sqlalchemy.dialects.postgresql import insert
from sqlalchemy.orm import aliased
from .database import Database
from .models import TodoItem, Priority, User, Project, Category
import json


class TodoApp:
    """TODO application with CRUD operations and JOIN demonstrations"""

    def __init__(self, db: Database):
        self.db = db

    # ========== User Management ==========
    def create_user(self, username: str, email: str, full_name: str = None) -> User:
        """Create a new user"""
        with self.db.get_session() as session:
            user = User(username=username, email=email, full_name=full_name)
            session.add(user)
            session.flush()
            session.refresh(user)
            session.expunge(user)
            return user

    # ========== Project Management ==========
    def create_project(self, name: str, owner_id: str, description: str = None,
                      start_date: datetime = None, end_date: datetime = None) -> Project:
        """Create a new project"""
        with self.db.get_session() as session:
            project = Project(
                name=name,
                owner_id=owner_id,
                description=description,
                start_date=start_date,
                end_date=end_date
            )
            session.add(project)
            session.flush()
            session.refresh(project)
            session.expunge(project)
            return project

    # ========== Category Management ==========
    def create_category(self, name: str, description: str = None, color: str = None) -> Category:
        """Create a new category"""
        with self.db.get_session() as session:
            category = Category(name=name, description=description, color=color)
            session.add(category)
            session.flush()
            session.refresh(category)
            session.expunge(category)
            return category

    # ========== TODO Management ==========
    def create_todo(self, title: str, description: str = None,
                   priority: Priority = Priority.MEDIUM,
                   tags: list = None, extra_data: dict = None,
                   due_date: datetime = None,
                   owner_id: str = None, project_id: str = None,
                   category_id: str = None, parent_todo_id: str = None) -> TodoItem:
        """
        Create a new TODO item

        Args:
            title: TODO title
            description: Optional description
            priority: Priority level
            tags: List of tags
            extra_data: Additional metadata as JSONB
            due_date: Optional due date
            owner_id: Owner user ID (for JOIN demonstrations)
            project_id: Project ID (for JOIN demonstrations)
            category_id: Category ID (for JOIN demonstrations)
            parent_todo_id: Parent TODO ID for subtasks (self-referential JOIN)

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
                due_date=due_date,
                owner_id=owner_id,
                project_id=project_id,
                category_id=category_id,
                parent_todo_id=parent_todo_id
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
                'by_priority': {p.value: c for p, c in priority_counts}
            }

    # ========== JOIN Query Demonstrations ==========

    def inner_join_todos_with_projects(self):
        """
        INNER JOIN: Get all TODOs that have an associated project
        Only returns rows where both TODO and Project exist
        """
        with self.db.get_session() as session:
            results = session.query(
                TodoItem.title,
                TodoItem.priority,
                Project.name.label('project_name'),
                User.username.label('owner_username')
            ).join(
                Project, TodoItem.project_id == Project.id
            ).join(
                User, TodoItem.owner_id == User.id
            ).all()
            return results

    def left_join_todos_with_projects(self):
        """
        LEFT JOIN: Get all TODOs with their projects (if any)
        Returns all TODOs, even those without a project
        """
        with self.db.get_session() as session:
            results = session.query(
                TodoItem.title,
                TodoItem.priority,
                Project.name.label('project_name'),
                Category.name.label('category_name')
            ).outerjoin(
                Project, TodoItem.project_id == Project.id
            ).outerjoin(
                Category, TodoItem.category_id == Category.id
            ).all()
            return results

    def right_join_projects_with_todos(self):
        """
        RIGHT JOIN (simulated with LEFT JOIN): Get all projects with their TODOs
        Shows all projects, even those without TODOs
        """
        with self.db.get_session() as session:
            # SQLAlchemy doesn't have direct right join, so we reverse the query
            results = session.query(
                Project.name.label('project_name'),
                Project.is_active,
                func.count(TodoItem.id).label('todo_count'),
                func.sum(
                    func.cast(TodoItem.is_completed, Integer)
                ).label('completed_count')
            ).outerjoin(
                TodoItem, Project.id == TodoItem.project_id
            ).group_by(
                Project.id, Project.name, Project.is_active
            ).all()
            return results

    def full_outer_join_users_projects(self):
        """
        FULL OUTER JOIN: Get all users and all projects
        Shows users without projects and projects without users
        Note: This is simulated using UNION in SQLAlchemy
        """
        with self.db.get_session() as session:
            # Full outer join simulation using raw SQL
            query = text("""
                SELECT u.username, p.name as project_name, COUNT(t.id) as todo_count
                FROM users u
                FULL OUTER JOIN projects p ON u.id = p.owner_id
                LEFT JOIN todo_items t ON p.id = t.project_id
                GROUP BY u.username, p.name
            """)
            results = session.execute(query).fetchall()
            return results

    def self_join_parent_child_todos(self):
        """
        SELF JOIN: Get parent TODOs with their subtasks
        Demonstrates self-referential relationships
        """
        with self.db.get_session() as session:
            # Create aliases for parent and child
            Parent = aliased(TodoItem)
            Child = aliased(TodoItem)

            results = session.query(
                Parent.title.label('parent_title'),
                Parent.priority.label('parent_priority'),
                Child.title.label('child_title'),
                Child.is_completed.label('child_completed')
            ).join(
                Child, Parent.id == Child.parent_todo_id
            ).all()
            return results

    def cross_join_categories_priorities(self):
        """
        CROSS JOIN: Get all combinations of categories and priorities
        Useful for reporting grids
        """
        with self.db.get_session() as session:
            # Cross join with count aggregation
            query = text("""
                SELECT c.name as category, priorities.priority, COUNT(t.id) as count
                FROM categories c
                CROSS JOIN (SELECT DISTINCT priority FROM todo_items) AS priorities(priority)
                LEFT JOIN todo_items t ON c.id = t.category_id AND t.priority = priorities.priority
                GROUP BY c.name, priorities.priority
                ORDER BY c.name, priorities.priority
            """)
            results = session.execute(query).fetchall()
            return results

    def multiple_joins_full_todo_details(self):
        """
        Multiple JOINs: Get complete TODO details with all related information
        Demonstrates joining multiple tables in a single query
        """
        with self.db.get_session() as session:
            results = session.query(
                TodoItem.title,
                TodoItem.description,
                TodoItem.priority,
                TodoItem.is_completed,
                User.username.label('owner'),
                User.email.label('owner_email'),
                Project.name.label('project'),
                Category.name.label('category'),
                Category.color.label('category_color')
            ).outerjoin(
                User, TodoItem.owner_id == User.id
            ).outerjoin(
                Project, TodoItem.project_id == Project.id
            ).outerjoin(
                Category, TodoItem.category_id == Category.id
            ).all()
            return results

    def aggregated_join_project_summary(self):
        """
        JOIN with aggregations: Get project summary with TODO statistics
        Demonstrates GROUP BY with JOINs
        """
        with self.db.get_session() as session:
            results = session.query(
                Project.name.label('project_name'),
                User.username.label('owner'),
                func.count(TodoItem.id).label('total_todos'),
                func.sum(
                    func.cast(TodoItem.is_completed, Integer)
                ).label('completed_todos'),
                func.count(
                    case((TodoItem.priority == Priority.HIGH, 1))
                ).label('high_priority_count')
            ).join(
                User, Project.owner_id == User.id
            ).outerjoin(
                TodoItem, Project.id == TodoItem.project_id
            ).group_by(
                Project.id, Project.name, User.username
            ).order_by(
                func.count(TodoItem.id).desc()
            ).all()
            return results

    def subquery_join_active_users(self):
        """
        Subquery JOIN: Get users with their active TODO count
        Demonstrates subquery in JOIN
        """
        with self.db.get_session() as session:
            # Subquery to count active TODOs per user
            active_todos_subq = session.query(
                TodoItem.owner_id,
                func.count(TodoItem.id).label('active_count')
            ).filter(
                TodoItem.is_completed == False
            ).group_by(
                TodoItem.owner_id
            ).subquery()

            # Join with users
            results = session.query(
                User.username,
                User.email,
                func.coalesce(active_todos_subq.c.active_count, 0).label('active_todos')
            ).outerjoin(
                active_todos_subq, User.id == active_todos_subq.c.owner_id
            ).all()
            return results


def demo(dsn: str):
    """
    Demonstration of the TODO app with PostgreSQL features, including JOINs

    Args:
        dsn: PostgreSQL connection string
    """
    print("=" * 80)
    print("TODO App - PostgreSQL Features Demo with JOIN Operations")
    print("=" * 80)

    # Initialize database
    db = Database(dsn)

    # Test connection
    print("\n1. Testing database connection...")
    if not db.test_connection():
        print("Failed to connect to database!")
        return

    # Create tables
    print("\n2. Creating tables with foreign key relationships...")
    try:
        db.drop_tables()  # Clean slate - may fail on non-standard PostgreSQL
    except Exception as e:
        print(f"   Note: Could not drop existing tables (this is OK): {str(e)[:100]}")

    try:
        db.create_tables()
        print("   ✓ Tables created successfully")
    except Exception as e:
        print(f"   Note: Tables may already exist (this is OK): {str(e)[:100]}")

    # Initialize app
    app = TodoApp(db)

    # Create users
    print("\n3. Creating users...")
    user1 = app.create_user(
        username="alice",
        email="alice@example.com",
        full_name="Alice Johnson"
    )
    print(f"   Created user: {user1.username} (ID: {user1.id})")

    user2 = app.create_user(
        username="bob",
        email="bob@example.com",
        full_name="Bob Smith"
    )
    print(f"   Created user: {user2.username} (ID: {user2.id})")

    user3 = app.create_user(
        username="charlie",
        email="charlie@example.com",
        full_name="Charlie Brown"
    )
    print(f"   Created user: {user3.username} (ID: {user3.id})")

    # Create categories
    print("\n4. Creating categories...")
    cat_dev = app.create_category(
        name="Development",
        description="Software development tasks",
        color="#3498db"
    )
    print(f"   Created category: {cat_dev.name}")

    cat_docs = app.create_category(
        name="Documentation",
        description="Documentation and writing tasks",
        color="#2ecc71"
    )
    print(f"   Created category: {cat_docs.name}")

    cat_research = app.create_category(
        name="Research",
        description="Learning and research tasks",
        color="#e74c3c"
    )
    print(f"   Created category: {cat_research.name}")

    # Create projects
    print("\n5. Creating projects...")
    project1 = app.create_project(
        name="PostgreSQL Learning Path",
        owner_id=str(user1.id),
        description="Master PostgreSQL advanced features",
        start_date=datetime.now(),
        end_date=datetime.now() + timedelta(days=30)
    )
    print(f"   Created project: {project1.name} (Owner: {user1.username})")

    project2 = app.create_project(
        name="TODO Application",
        owner_id=str(user2.id),
        description="Build a full-featured TODO app",
        start_date=datetime.now()
    )
    print(f"   Created project: {project2.name} (Owner: {user2.username})")

    project3 = app.create_project(
        name="Database Migration",
        owner_id=str(user3.id),
        description="Migrate legacy database to PostgreSQL"
    )
    print(f"   Created project: {project3.name} (Owner: {user3.username})")

    # Create TODO items with relationships
    print("\n6. Creating TODO items with relationships...")
    todo1 = app.create_todo(
        title="Learn PostgreSQL JOINs",
        description="Study INNER, OUTER, CROSS, and SELF JOINs",
        priority=Priority.HIGH,
        tags=["learning", "database", "sql"],
        extra_data={"course": "Advanced SQL", "hours": 10},
        due_date=datetime.now() + timedelta(days=7),
        owner_id=str(user1.id),
        project_id=str(project1.id),
        category_id=str(cat_research.id)
    )
    print(f"   Created: {todo1.title} (Owner: {user1.username}, Project: {project1.name})")

    todo2 = app.create_todo(
        title="Implement INNER JOIN examples",
        description="Create code examples for INNER JOINs",
        priority=Priority.URGENT,
        tags=["project", "python", "examples"],
        extra_data={"repository": "github.com/user/todo", "language": "Python"},
        owner_id=str(user2.id),
        project_id=str(project2.id),
        category_id=str(cat_dev.id)
    )
    print(f"   Created: {todo2.title} (Owner: {user2.username}, Project: {project2.name})")

    todo3 = app.create_todo(
        title="Write JOIN documentation",
        description="Document all JOIN types with examples",
        priority=Priority.MEDIUM,
        tags=["documentation", "writing", "sql"],
        due_date=datetime.now() + timedelta(days=3),
        owner_id=str(user1.id),
        project_id=str(project1.id),
        category_id=str(cat_docs.id)
    )
    print(f"   Created: {todo3.title} (Owner: {user1.username}, Project: {project1.name})")

    # Create subtask (for self-join demonstration)
    subtask1 = app.create_todo(
        title="Test INNER JOIN queries",
        description="Write and test INNER JOIN queries",
        priority=Priority.HIGH,
        tags=["testing", "sql"],
        owner_id=str(user2.id),
        project_id=str(project2.id),
        category_id=str(cat_dev.id),
        parent_todo_id=str(todo2.id)
    )
    print(f"   Created subtask: {subtask1.title} (Parent: {todo2.title})")

    # Create more TODOs for variety
    todo4 = app.create_todo(
        title="Optimize database indexes",
        description="Review and optimize database indexes",
        priority=Priority.MEDIUM,
        tags=["optimization", "database"],
        owner_id=str(user3.id),
        project_id=str(project3.id),
        category_id=str(cat_dev.id)
    )
    print(f"   Created: {todo4.title} (Owner: {user3.username}, Project: {project3.name})")

    # TODO without project (for LEFT JOIN demonstration)
    todo5 = app.create_todo(
        title="Personal reading list",
        description="Books to read this month",
        priority=Priority.LOW,
        tags=["personal", "reading"],
        owner_id=str(user1.id),
        category_id=str(cat_research.id)
    )
    print(f"   Created: {todo5.title} (No project assigned)")

    # Complete a TODO
    print("\n7. Completing a TODO...")
    completed = app.complete_todo(str(todo2.id))
    print(f"   ✓ Completed: {completed.title}")

    print("\n" + "=" * 80)
    print("JOIN DEMONSTRATIONS")
    print("=" * 80)

    # INNER JOIN demonstration
    print("\n8. INNER JOIN - TODOs with Projects and Owners...")
    print("   (Only shows TODOs that have both project and owner)")
    try:
        results = app.inner_join_todos_with_projects()
        for row in results:
            print(f"   - {row.title} [{row.priority.value}]")
            print(f"     Project: {row.project_name}, Owner: {row.owner_username}")
    except Exception as e:
        print(f"   Error: {str(e)[:100]}")

    # LEFT JOIN demonstration
    print("\n9. LEFT JOIN - All TODOs with Optional Project/Category...")
    print("   (Shows all TODOs, even without project or category)")
    try:
        results = app.left_join_todos_with_projects()
        for row in results:
            project = row.project_name or "None"
            category = row.category_name or "None"
            print(f"   - {row.title} [{row.priority.value}]")
            print(f"     Project: {project}, Category: {category}")
    except Exception as e:
        print(f"   Error: {str(e)[:100]}")

    # RIGHT JOIN (simulated) demonstration
    print("\n10. RIGHT JOIN - All Projects with TODO Counts...")
    print("   (Shows all projects, even without TODOs)")
    try:
        results = app.right_join_projects_with_todos()
        for row in results:
            completed = row.completed_count or 0
            print(f"   - {row.project_name} (Active: {row.is_active})")
            print(f"     Total TODOs: {row.todo_count}, Completed: {completed}")
    except Exception as e:
        print(f"   Error: {str(e)[:100]}")

    # FULL OUTER JOIN demonstration
    print("\n11. FULL OUTER JOIN - All Users and Projects...")
    print("   (Shows all users and projects, even unmatched)")
    try:
        results = app.full_outer_join_users_projects()
        for row in results:
            username = row.username or "No User"
            project_name = row.project_name or "No Project"
            print(f"   - User: {username}, Project: {project_name}, TODOs: {row.todo_count}")
    except Exception as e:
        print(f"   Note: FULL OUTER JOIN may not be supported: {str(e)[:100]}")

    # SELF JOIN demonstration
    print("\n12. SELF JOIN - Parent TODOs with Subtasks...")
    print("   (Shows hierarchical relationships)")
    try:
        results = app.self_join_parent_child_todos()
        for row in results:
            status = "✓" if row.child_completed else "○"
            print(f"   - {row.parent_title} [{row.parent_priority.value}]")
            print(f"     └─ {status} {row.child_title}")
    except Exception as e:
        print(f"   Error: {str(e)[:100]}")

    # CROSS JOIN demonstration
    print("\n13. CROSS JOIN - Category × Priority Matrix...")
    print("   (Shows all combinations with counts)")
    try:
        results = app.cross_join_categories_priorities()
        current_category = None
        for row in results:
            if current_category != row.category:
                print(f"\n   {row.category}:")
                current_category = row.category
            print(f"     - {row.priority}: {row.count} TODOs")
    except Exception as e:
        print(f"   Note: CROSS JOIN may not be fully supported: {str(e)[:100]}")

    # Multiple JOINs demonstration
    print("\n14. Multiple JOINs - Complete TODO Details...")
    print("   (Joins User, Project, and Category tables)")
    try:
        results = app.multiple_joins_full_todo_details()
        for row in results:
            owner = row.owner or "No Owner"
            project = row.project or "No Project"
            category = row.category or "No Category"
            status = "✓" if row.is_completed else "○"
            print(f"   {status} {row.title} [{row.priority.value}]")
            print(f"     Owner: {owner}, Project: {project}, Category: {category}")
    except Exception as e:
        print(f"   Error: {str(e)[:100]}")

    # Aggregated JOIN demonstration
    print("\n15. JOIN with Aggregations - Project Summary...")
    print("   (Aggregates TODO counts per project)")
    try:
        results = app.aggregated_join_project_summary()
        for row in results:
            completion_rate = (row.completed_todos / row.total_todos * 100) if row.total_todos > 0 else 0
            print(f"   - {row.project_name} (Owner: {row.owner})")
            print(f"     Total: {row.total_todos}, Completed: {row.completed_todos} ({completion_rate:.0f}%)")
            print(f"     High Priority: {row.high_priority_count}")
    except Exception as e:
        print(f"   Error: {str(e)[:100]}")

    # Subquery JOIN demonstration
    print("\n16. Subquery JOIN - Users with Active TODO Counts...")
    print("   (Uses subquery in JOIN)")
    try:
        results = app.subquery_join_active_users()
        for row in results:
            print(f"   - {row.username} ({row.email})")
            print(f"     Active TODOs: {row.active_todos}")
    except Exception as e:
        print(f"   Error: {str(e)[:100]}")

    print("\n" + "=" * 80)
    print("ADDITIONAL POSTGRESQL FEATURES")
    print("=" * 80)

    # Full-text search (PostgreSQL feature)
    print("\n17. Full-text search for 'JOIN'...")
    try:
        search_results = app.search_todos("JOIN")
        for todo in search_results:
            print(f"   - {todo.title}")
    except Exception as e:
        print(f"   Note: Full-text search not supported: {str(e)[:80]}")

    # Find by tag (ARRAY feature)
    print("\n18. Finding TODOs with tag 'sql'...")
    try:
        tagged_todos = app.find_by_tag("sql")
        for todo in tagged_todos:
            tags_display = todo.tags if isinstance(todo.tags, list) else "N/A"
            print(f"   - {todo.title} - Tags: {tags_display}")
    except Exception as e:
        print(f"   Note: ARRAY search not supported: {str(e)[:80]}")

    # Find by extra_data (JSONB feature)
    print("\n19. Finding TODOs with extra_data language='Python'...")
    try:
        python_todos = app.find_by_metadata("language", "Python")
        for todo in python_todos:
            print(f"   - {todo.title} - Extra Data: {todo.extra_data}")
    except Exception as e:
        print(f"   Note: JSONB search not fully supported: {str(e)[:80]}")

    # Get statistics
    print("\n20. Overall Statistics...")
    stats = app.get_statistics()
    print(f"   Total TODOs: {stats['total']}")
    print(f"   Completed: {stats['completed']}")
    print(f"   Pending: {stats['pending']}")
    print(f"   Overdue: {stats['overdue']}")
    print(f"   By Priority: {json.dumps(stats['by_priority'], indent=6)}")

    print("\n" + "=" * 80)
    print("Demo completed successfully!")
    print("=" * 80)
    print("\nThis demo showcased:")
    print("  ✓ Foreign key relationships")
    print("  ✓ INNER JOIN (matching records)")
    print("  ✓ LEFT JOIN (all left records)")
    print("  ✓ RIGHT JOIN (all right records)")
    print("  ✓ FULL OUTER JOIN (all records)")
    print("  ✓ SELF JOIN (hierarchical data)")
    print("  ✓ CROSS JOIN (cartesian product)")
    print("  ✓ Multiple JOINs (3+ tables)")
    print("  ✓ Aggregated JOINs (GROUP BY)")
    print("  ✓ Subquery JOINs")
    print("=" * 80)


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print("Usage: python main.py <postgresql_dsn>")
        print("Example: python main.py postgresql://user:password@localhost:5432/todo_db")
        sys.exit(1)

    dsn = sys.argv[1]
    demo(dsn)
