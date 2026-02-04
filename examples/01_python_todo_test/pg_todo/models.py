"""
SQLAlchemy models demonstrating PostgreSQL features
"""
from datetime import datetime
from sqlalchemy import (
    Column, String, Text, DateTime, Boolean, Integer,
    Index, Enum as SQLEnum, CheckConstraint, ForeignKey
)
from sqlalchemy.dialects.postgresql import UUID, JSONB, ARRAY, TSVECTOR
from sqlalchemy.ext.declarative import declarative_base
from sqlalchemy.orm import relationship
from sqlalchemy.sql import func
import uuid
import enum

Base = declarative_base()


class Priority(enum.Enum):
    """Priority levels for TODO items"""
    LOW = "low"
    MEDIUM = "medium"
    HIGH = "high"
    URGENT = "urgent"


class User(Base):
    """
    User model for multi-user support
    Demonstrates foreign key relationships with TodoItem
    """
    __tablename__ = 'users'

    id = Column(UUID(as_uuid=True), primary_key=True, default=uuid.uuid4)
    username = Column(String(50), unique=True, nullable=False)
    email = Column(String(100), unique=True, nullable=False)
    full_name = Column(String(200))
    created_at = Column(DateTime(timezone=True), server_default=func.now(), nullable=False)

    # Relationships
    todos = relationship('TodoItem', back_populates='owner', cascade='all, delete-orphan')
    projects = relationship('Project', back_populates='owner', cascade='all, delete-orphan')

    __table_args__ = (
        Index('idx_username', 'username'),
        Index('idx_email', 'email'),
    )

    def __repr__(self):
        return f"<User(id={self.id}, username='{self.username}')>"


class Project(Base):
    """
    Project model to group TODO items
    Demonstrates one-to-many relationships
    """
    __tablename__ = 'projects'

    id = Column(UUID(as_uuid=True), primary_key=True, default=uuid.uuid4)
    name = Column(String(200), nullable=False)
    description = Column(Text)
    owner_id = Column(UUID(as_uuid=True), ForeignKey('users.id', ondelete='CASCADE'), nullable=False)
    start_date = Column(DateTime(timezone=True))
    end_date = Column(DateTime(timezone=True))
    created_at = Column(DateTime(timezone=True), server_default=func.now(), nullable=False)
    is_active = Column(Boolean, default=True, nullable=False)

    # Relationships
    owner = relationship('User', back_populates='projects')
    todos = relationship('TodoItem', back_populates='project')

    __table_args__ = (
        Index('idx_project_owner', 'owner_id'),
        Index('idx_project_active', 'is_active'),
    )

    def __repr__(self):
        return f"<Project(id={self.id}, name='{self.name}')>"


class Category(Base):
    """
    Category model for classifying TODO items
    Demonstrates many-to-many relationships via association table
    """
    __tablename__ = 'categories'

    id = Column(UUID(as_uuid=True), primary_key=True, default=uuid.uuid4)
    name = Column(String(100), unique=True, nullable=False)
    description = Column(Text)
    color = Column(String(7))  # Hex color code
    created_at = Column(DateTime(timezone=True), server_default=func.now(), nullable=False)

    # Relationships
    todos = relationship('TodoItem', back_populates='category')

    def __repr__(self):
        return f"<Category(id={self.id}, name='{self.name}')>"


class TodoItem(Base):
    """
    TODO item model demonstrating PostgreSQL features:
    - UUID primary key
    - JSONB type for metadata storage
    - ARRAY type for tags
    - Full-text search (TSVECTOR)
    - Enum types
    - Automatic timestamps
    - Composite indexes
    - Foreign key relationships (for JOIN demonstrations)
    """
    __tablename__ = 'todo_items'

    # UUID primary key (PostgreSQL feature)
    id = Column(UUID(as_uuid=True), primary_key=True, default=uuid.uuid4)

    # Basic fields
    title = Column(String(200), nullable=False)
    description = Column(Text)

    # Enum type (PostgreSQL feature)
    priority = Column(SQLEnum(Priority), default=Priority.MEDIUM, nullable=False)

    # Boolean field
    is_completed = Column(Boolean, default=False, nullable=False)

    # Foreign keys for relationships (demonstrates JOINs)
    owner_id = Column(UUID(as_uuid=True), ForeignKey('users.id', ondelete='SET NULL'))
    project_id = Column(UUID(as_uuid=True), ForeignKey('projects.id', ondelete='SET NULL'))
    category_id = Column(UUID(as_uuid=True), ForeignKey('categories.id', ondelete='SET NULL'))
    parent_todo_id = Column(UUID(as_uuid=True), ForeignKey('todo_items.id', ondelete='SET NULL'))  # Self-referential

    # ARRAY type for tags (PostgreSQL feature)
    tags = Column(ARRAY(String), default=list)

    # JSONB type for additional metadata (PostgreSQL feature)
    extra_data = Column(JSONB, default=dict)

    # Full-text search vector (PostgreSQL feature)
    search_vector = Column(TSVECTOR)

    # Automatic timestamps
    created_at = Column(DateTime(timezone=True), server_default=func.now(), nullable=False)
    updated_at = Column(DateTime(timezone=True), server_default=func.now(), onupdate=func.now(), nullable=False)

    # Due date
    due_date = Column(DateTime(timezone=True))

    # Completion date
    completed_at = Column(DateTime(timezone=True))

    # Relationships
    owner = relationship('User', back_populates='todos')
    project = relationship('Project', back_populates='todos')
    category = relationship('Category', back_populates='todos')
    parent_todo = relationship('TodoItem', remote_side=[id], backref='subtasks')

    # Constraints and indexes
    __table_args__ = (
        # Composite indexes
        Index('idx_priority_completed', 'priority', 'is_completed'),
        Index('idx_created_at', 'created_at'),
        Index('idx_due_date', 'due_date'),
        # Foreign key indexes (improves JOIN performance)
        Index('idx_owner_id', 'owner_id'),
        Index('idx_project_id', 'project_id'),
        Index('idx_category_id', 'category_id'),
        Index('idx_parent_todo_id', 'parent_todo_id'),
        # Composite index for common JOIN query patterns
        Index('idx_owner_project', 'owner_id', 'project_id'),
        Index('idx_project_completed', 'project_id', 'is_completed'),
        # GIN indexes for JSONB and ARRAY (PostgreSQL feature)
        Index('idx_tags_gin', 'tags', postgresql_using='gin'),
        Index('idx_extra_data_gin', 'extra_data', postgresql_using='gin'),
        # GIN index for full-text search (PostgreSQL feature)
        Index('idx_search_vector', 'search_vector', postgresql_using='gin'),
        # Check constraint
        CheckConstraint(
            '(is_completed = false) OR (completed_at IS NOT NULL)',
            name='check_completed_at'
        ),
    )

    def __repr__(self):
        return f"<TodoItem(id={self.id}, title='{self.title}', priority={self.priority.value})>"

    def to_dict(self):
        """Convert to dictionary"""
        return {
            'id': str(self.id),
            'title': self.title,
            'description': self.description,
            'priority': self.priority.value,
            'is_completed': self.is_completed,
            'tags': self.tags or [],
            'extra_data': self.extra_data or {},
            'created_at': self.created_at.isoformat() if self.created_at else None,
            'updated_at': self.updated_at.isoformat() if self.updated_at else None,
            'due_date': self.due_date.isoformat() if self.due_date else None,
            'completed_at': self.completed_at.isoformat() if self.completed_at else None,
        }
