from django.db.backends.postgresql.operations import DatabaseOperations as PGOperations


class DatabaseOperations(PGOperations):
    """Operations override for db9 test-harness gaps."""

    def sequence_reset_sql(self, style, model_list):
        # db9 GAP (issue #2707): pg_get_serial_sequence() is unknown in the
        # setval() argument context, so Django's post-migrate / loaddata
        # sequence reset (`SELECT setval(pg_get_serial_sequence(...), ...)`)
        # errors. Skipping it only affects autoincrement-counter determinism
        # after bulk inserts (a small set of fixture tests), not schema setup.
        return []

    def sequence_reset_by_name_sql(self, style, sequences):
        # Same db9 gap as sequence_reset_sql (#2707).
        return []
