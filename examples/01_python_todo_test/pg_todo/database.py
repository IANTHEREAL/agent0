"""
Database connection and session management
"""
from sqlalchemy import create_engine, text
from sqlalchemy.orm import sessionmaker, Session
from sqlalchemy.pool import QueuePool
from contextlib import contextmanager
from .models import Base
import logging

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)


class Database:
    """Database management class"""

    def __init__(self, dsn: str):
        """
        Initialize database connection

        Args:
            dsn: PostgreSQL connection string in format:
                postgresql://user:password@host:port/database
                or postgres:// (will be auto-converted)
        """
        # SQLAlchemy 2.0+ requires 'postgresql://' not 'postgres://'
        if dsn.startswith('postgres://'):
            dsn = dsn.replace('postgres://', 'postgresql://', 1)
        self.dsn = dsn

        # Create engine with connection pooling (PostgreSQL feature)
        self.engine = create_engine(
            dsn,
            poolclass=QueuePool,
            pool_size=5,
            max_overflow=10,
            pool_pre_ping=True,  # Connection health check
            echo=False,  # Set to True to see SQL logs
        )

        # Create session factory
        self.SessionLocal = sessionmaker(
            autocommit=False,
            autoflush=False,
            bind=self.engine
        )

    def create_tables(self):
        """Create all tables"""
        Base.metadata.create_all(bind=self.engine)
        logger.info("Database tables created successfully")

        # Create full-text search trigger (PostgreSQL feature)
        self._create_search_trigger()

    def drop_tables(self):
        """Drop all tables"""
        Base.metadata.drop_all(bind=self.engine)
        logger.info("Database tables dropped successfully")

    def _create_search_trigger(self):
        """
        Create full-text search trigger (PostgreSQL feature)
        Automatically updates search_vector field
        """
        with self.engine.connect() as conn:
            # Create trigger function
            conn.execute(text("""
                CREATE OR REPLACE FUNCTION update_search_vector()
                RETURNS trigger AS $$
                BEGIN
                    NEW.search_vector :=
                        setweight(to_tsvector('english', COALESCE(NEW.title, '')), 'A') ||
                        setweight(to_tsvector('english', COALESCE(NEW.description, '')), 'B');
                    RETURN NEW;
                END;
                $$ LANGUAGE plpgsql;
            """))

            # Drop old trigger if exists
            conn.execute(text("""
                DROP TRIGGER IF EXISTS todo_search_vector_update ON todo_items;
            """))

            # Create trigger
            conn.execute(text("""
                CREATE TRIGGER todo_search_vector_update
                BEFORE INSERT OR UPDATE ON todo_items
                FOR EACH ROW
                EXECUTE FUNCTION update_search_vector();
            """))

            conn.commit()
            logger.info("Full-text search trigger created successfully")

    @contextmanager
    def get_session(self) -> Session:
        """
        Get database session context manager

        Usage:
            with db.get_session() as session:
                # use session
                pass
        """
        session = self.SessionLocal()
        try:
            yield session
            session.commit()
        except Exception as e:
            session.rollback()
            logger.error(f"Session rollback due to error: {e}")
            raise
        finally:
            session.close()

    def test_connection(self) -> bool:
        """Test database connection"""
        try:
            with self.engine.connect() as conn:
                result = conn.execute(text("SELECT version()"))
                version = result.scalar()
                logger.info(f"Connected to PostgreSQL: {version}")
                return True
        except Exception as e:
            logger.error(f"Failed to connect to database: {e}")
            return False
