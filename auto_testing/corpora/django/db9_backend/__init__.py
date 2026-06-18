# db9 Django database backend — thin override of django.db.backends.postgresql.
#
# db9 speaks the PostgreSQL wire protocol, so Django's stock `postgresql` backend
# works against it directly. This package exists ONLY to carry a small, explicit
# set of workarounds for db9 gaps that block Django's TEST HARNESS (not its query
# logic). Each override is a logged db9 gap with its tracking issue; when the gap
# is fixed in db9, the corresponding override is deleted and we fall back to stock
# postgresql behavior. Nothing here masks query/semantic differences — those are
# recorded as failures in Django-bank.md, never hidden.
