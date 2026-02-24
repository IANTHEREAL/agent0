"""
Database connection and trigger setup for db9.

Sets up:
1. HTTP extension (CREATE EXTENSION http)
2. Trigger functions that POST to a webhook URL via extensions.http_post()
3. AFTER triggers on the products table for INSERT, UPDATE, DELETE
"""

from contextlib import contextmanager

from sqlalchemy import create_engine, text
from sqlalchemy.orm import sessionmaker

from .models import Base


class Database:
    def __init__(self, dsn: str):
        self.engine = create_engine(dsn, echo=False, pool_pre_ping=True)
        self.Session = sessionmaker(bind=self.engine)

    @contextmanager
    def session(self):
        s = self.Session()
        try:
            yield s
            s.commit()
        except Exception:
            s.rollback()
            raise
        finally:
            s.close()

    def setup_tables(self):
        """Create the products and webhook_log tables."""
        # Use raw SQL with IF EXISTS for idempotent cleanup —
        # SQLAlchemy's drop_all() omits IF EXISTS, which fails on db9
        # when the table doesn't exist yet.
        with self.engine.connect() as conn:
            conn.execute(text("DROP TRIGGER IF EXISTS trg_product_insert ON products"))
            conn.execute(text("DROP TRIGGER IF EXISTS trg_product_update ON products"))
            conn.execute(text("DROP TRIGGER IF EXISTS trg_product_delete ON products"))
            conn.execute(text("DROP FUNCTION IF EXISTS notify_product_insert()"))
            conn.execute(text("DROP FUNCTION IF EXISTS notify_product_update()"))
            conn.execute(text("DROP FUNCTION IF EXISTS notify_product_delete()"))
            conn.execute(text("DROP TABLE IF EXISTS webhook_log"))
            conn.execute(text("DROP TABLE IF EXISTS products"))
            conn.commit()
        Base.metadata.create_all(self.engine)

    def setup_http_extension(self):
        """Enable the db9 HTTP extension."""
        with self.engine.connect() as conn:
            conn.execute(text("CREATE EXTENSION IF NOT EXISTS http"))
            conn.commit()

    def setup_triggers(self, webhook_url: str):
        """
        Create trigger functions and attach them to the products table.

        Each trigger function:
        1. Builds a JSON payload with the event type and row data
        2. POSTs it to the webhook URL via extensions.http_post()
        3. Logs the result (HTTP status) into webhook_log

        Since db9 does not support TG_OP / TG_TABLE_NAME special variables,
        we create one trigger function per event type.
        """
        with self.engine.connect() as conn:
            # --- Trigger function for INSERT ---
            conn.execute(text(f"""
                CREATE OR REPLACE FUNCTION notify_product_insert()
                RETURNS TRIGGER AS $$
                BEGIN
                    INSERT INTO webhook_log (event_type, table_name, payload, http_status)
                    SELECT
                        'INSERT',
                        'products',
                        '{{"event":"INSERT","id":' || NEW.id::text || ',"name":"' || NEW.name || '","price":' || NEW.price::text || '}}',
                        status
                    FROM extensions.http_post(
                        '{webhook_url}',
                        '{{"event":"INSERT","id":' || NEW.id::text || ',"name":"' || NEW.name || '","price":' || NEW.price::text || '}}',
                        'application/json'
                    );
                    RETURN NEW;
                END;
                $$ LANGUAGE plpgsql
            """))

            # --- Trigger function for UPDATE ---
            conn.execute(text(f"""
                CREATE OR REPLACE FUNCTION notify_product_update()
                RETURNS TRIGGER AS $$
                BEGIN
                    INSERT INTO webhook_log (event_type, table_name, payload, http_status)
                    SELECT
                        'UPDATE',
                        'products',
                        '{{"event":"UPDATE","id":' || NEW.id::text || ',"name":"' || NEW.name || '","price":' || NEW.price::text || '}}',
                        status
                    FROM extensions.http_post(
                        '{webhook_url}',
                        '{{"event":"UPDATE","id":' || NEW.id::text || ',"name":"' || NEW.name || '","price":' || NEW.price::text || '}}',
                        'application/json'
                    );
                    RETURN NEW;
                END;
                $$ LANGUAGE plpgsql
            """))

            # --- Trigger function for DELETE ---
            conn.execute(text(f"""
                CREATE OR REPLACE FUNCTION notify_product_delete()
                RETURNS TRIGGER AS $$
                BEGIN
                    INSERT INTO webhook_log (event_type, table_name, payload, http_status)
                    SELECT
                        'DELETE',
                        'products',
                        '{{"event":"DELETE","id":' || OLD.id::text || ',"name":"' || OLD.name || '"}}',
                        status
                    FROM extensions.http_post(
                        '{webhook_url}',
                        '{{"event":"DELETE","id":' || OLD.id::text || ',"name":"' || OLD.name || '"}}',
                        'application/json'
                    );
                    RETURN OLD;
                END;
                $$ LANGUAGE plpgsql
            """))

            # --- Attach triggers to the products table ---
            conn.execute(text("""
                CREATE TRIGGER trg_product_insert
                    AFTER INSERT ON products
                    FOR EACH ROW EXECUTE FUNCTION notify_product_insert()
            """))

            conn.execute(text("""
                CREATE TRIGGER trg_product_update
                    AFTER UPDATE ON products
                    FOR EACH ROW EXECUTE FUNCTION notify_product_update()
            """))

            conn.execute(text("""
                CREATE TRIGGER trg_product_delete
                    AFTER DELETE ON products
                    FOR EACH ROW EXECUTE FUNCTION notify_product_delete()
            """))

            conn.commit()

    def setup_all(self, webhook_url: str):
        """Run the full setup: tables, extension, triggers."""
        self.setup_tables()
        self.setup_http_extension()
        self.setup_triggers(webhook_url)

    def cleanup(self):
        """Drop triggers, functions, and tables."""
        with self.engine.connect() as conn:
            conn.execute(text("DROP TRIGGER IF EXISTS trg_product_insert ON products"))
            conn.execute(text("DROP TRIGGER IF EXISTS trg_product_update ON products"))
            conn.execute(text("DROP TRIGGER IF EXISTS trg_product_delete ON products"))
            conn.execute(text("DROP FUNCTION IF EXISTS notify_product_insert()"))
            conn.execute(text("DROP FUNCTION IF EXISTS notify_product_update()"))
            conn.execute(text("DROP FUNCTION IF EXISTS notify_product_delete()"))
            conn.execute(text("DROP TABLE IF EXISTS webhook_log"))
            conn.execute(text("DROP TABLE IF EXISTS products"))
            conn.commit()
