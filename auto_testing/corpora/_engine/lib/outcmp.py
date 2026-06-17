"""
Canonical formatting + hashing of a query result, sqllogictest-style, so db9's
output and PostgreSQL's output can be compared deterministically.

The whole result is flattened to a list of value-strings (one per cell), each
formatted by its declared column type (I=int, R=real %.3f, T=text), then ordered
per the sort mode, then md5-hashed. Because BOTH db9 and PG outputs go through
the identical normaliser, the comparison is fair even where the formatting is
lossy (e.g. %.3f gives float tolerance for avg()-style aggregates).
"""
import hashlib
from decimal import Decimal


def fmt_val(v, t):
    if v is None:
        return "NULL"
    if t == "I":
        try:
            return str(int(v))
        except (ValueError, TypeError):
            return str(v)
    if t == "R":
        try:
            return "%.3f" % float(v)
        except (ValueError, TypeError):
            return str(v)
    # text / unknown
    if isinstance(v, bool):
        return "1" if v else "0"
    if isinstance(v, (bytes, bytearray)):
        return v.hex()
    if isinstance(v, Decimal):
        v = format(v, "f")
    s = str(v)
    return s if s != "" else "(empty)"


def _flat(rows, types):
    out = []
    for row in rows:
        for j, val in enumerate(row):
            t = types[j] if j < len(types) else "T"
            out.append(fmt_val(val, t))
    return out


def normalize(rows, types, sort):
    vals = _flat(rows, types)
    if sort == "valuesort":
        vals = sorted(vals)
    elif sort == "rowsort":
        ncol = len(types) if types else (len(rows[0]) if rows else 1)
        ncol = max(ncol, 1)
        grouped = [vals[k:k + ncol] for k in range(0, len(vals), ncol)]
        grouped.sort()
        vals = [x for g in grouped for x in g]
    return vals


def result_hash(rows, types, sort):
    vals = normalize(rows, types, sort)
    if not vals:
        return "EMPTY"
    return hashlib.md5(("\n".join(vals) + "\n").encode("utf-8", "replace")).hexdigest()
