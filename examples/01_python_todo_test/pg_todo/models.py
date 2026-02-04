"""
SQLAlchemy models demonstrating PostgreSQL features
"""
from datetime import datetime
from sqlalchemy import (
    Column, String, Text, DateTime, Boolean, Integer,
    Index, Enum as SQLEnum, CheckConstraint
)
from sqlalchemy.dialects.postgresql import UUID, JSONB, ARRAY, TSVECTOR
from sqlalchemy.ext.declarative import declarative_base
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

    # Constraint: if completed, must have completion time
    __table_args__ = (
        # Composite indexes
        Index('idx_priority_completed', 'priority', 'is_completed'),
        Index('idx_created_at', 'created_at'),
        Index('idx_due_date', 'due_date'),
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
