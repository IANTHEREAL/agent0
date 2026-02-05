"""
SQLAlchemy models for the trigger webhook example.

Defines two tables:
- products: The main business table that triggers fire on
- webhook_log: Records each webhook HTTP call made by the trigger
"""

from sqlalchemy import Column, Integer, Text, Numeric, TIMESTAMP
from sqlalchemy.orm import DeclarativeBase
from sqlalchemy.sql import func


class Base(DeclarativeBase):
    pass


class Product(Base):
    """Product catalog table. AFTER triggers fire on INSERT/UPDATE/DELETE."""

    __tablename__ = "products"

    id = Column(Integer, primary_key=True, autoincrement=True)
    name = Column(Text, nullable=False)
    price = Column(Numeric(10, 2), nullable=False)
    category = Column(Text, nullable=False, server_default="general")
    created_at = Column(TIMESTAMP, server_default=func.now())
    updated_at = Column(TIMESTAMP, server_default=func.now())

    def __repr__(self):
        return f"<Product(id={self.id}, name='{self.name}', price={self.price})>"


class WebhookLog(Base):
    """
    Records webhook HTTP calls made by trigger functions.

    Each row represents one HTTP POST sent to the webhook endpoint
    when a product row is inserted, updated, or deleted.
    """

    __tablename__ = "webhook_log"

    id = Column(Integer, primary_key=True, autoincrement=True)
    event_type = Column(Text, nullable=False)
    table_name = Column(Text, nullable=False)
    payload = Column(Text)
    http_status = Column(Integer)
    created_at = Column(TIMESTAMP, server_default=func.now())

    def __repr__(self):
        return f"<WebhookLog(id={self.id}, event='{self.event_type}', status={self.http_status})>"
