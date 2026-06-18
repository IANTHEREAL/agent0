from django.db.backends.postgresql.base import DatabaseWrapper as PGDatabaseWrapper

from .operations import DatabaseOperations


class DatabaseWrapper(PGDatabaseWrapper):
    """db9 backend: stock postgresql + a few logged test-harness workarounds.

    `vendor` stays 'postgresql' on purpose — db9 is PostgreSQL-wire-compatible,
    so Django should treat it exactly like PG. Only the explicitly-overridden
    methods below differ, each tied to a db9 gap issue.
    """

    ops_class = DatabaseOperations

    def check_constraints(self, table_names=None):
        # db9 GAP (issue #2702): the parser rejects
        # `SET CONSTRAINTS ALL IMMEDIATE|DEFERRED`, which Django's TestCase
        # issues to force deferred-constraint checking. Until db9 parses it,
        # this is a no-op. db9 checks foreign keys eagerly, so for the common
        # case constraints are already validated; tests that specifically
        # exercise DEFERRED-then-violate behavior are recorded as failures,
        # not silently passed.
        return
