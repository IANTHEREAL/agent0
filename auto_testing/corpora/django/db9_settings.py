"""Django test-suite settings targeting db9 (host/port via env).

Used by run.sh:  python tests/runtests.py --settings=db9_settings <apps>
Both aliases use the db9_backend override package (see db9_backend/).
"""
import os

_HOST = os.environ.get("DB9_HOST", "127.0.0.1")
_PORT = os.environ.get("DB9_PORT", "5455")
_USER = os.environ.get("DB9_USER", "admin")
_PASS = os.environ.get("DB9_PASSWORD", "admin")


def _db(test_name):
    return {
        "ENGINE": "db9_backend",
        "NAME": os.environ.get("DB9_NAME", "postgres"),
        "USER": _USER,
        "PASSWORD": _PASS,
        "HOST": _HOST,
        "PORT": _PORT,
        "TEST": {"NAME": test_name},
    }


DATABASES = {
    "default": _db("test_db9_default"),
    "other": _db("test_db9_other"),
}
SECRET_KEY = "db9_django_compat_tests"
USE_TZ = False
