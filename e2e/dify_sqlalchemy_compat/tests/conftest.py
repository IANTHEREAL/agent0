from __future__ import annotations

import pytest

from dify_sqlalchemy_compat.harness import SCHEMA_NAME, flask_app_and_db_from_env, managed_schema


@pytest.fixture(scope="session")
def app_and_db():
    app, db = flask_app_and_db_from_env()
    try:
        yield app, db
    finally:
        with app.app_context():
            db.engine.dispose()


@pytest.fixture(scope="session")
def app(app_and_db):
    app, _db = app_and_db
    return app


@pytest.fixture(scope="session")
def db(app_and_db):
    _app, db = app_and_db
    return db


@pytest.fixture(autouse=True)
def app_context(app):
    with app.app_context():
        yield


@pytest.fixture(scope="session")
def engine(app, db):
    with app.app_context():
        engine = db.engine
    return engine


@pytest.fixture(scope="session")
def schema(engine):
    with managed_schema(engine, SCHEMA_NAME) as schema_name:
        yield schema_name
