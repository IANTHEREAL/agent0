"""
Parser for sqllogictest `.test` files.

Each file is a sequence of records:
  statement ok|error
  <sql...>                         (until blank line)

  query <types> [sort] [label]
  <sql...>                         (until a line that is exactly '----')
  ----
  <expected...>                    (until blank line) -- IGNORED here

We use PostgreSQL as the oracle (we run the SQL on PG + db9 and diff their
outputs), so the bundled expected block is consumed-and-discarded; we keep only
the query's column TYPES (I/T/R) and SORT mode (for canonical formatting).

`onlyif <db>` / `skipif <db>` prefixes are honoured (db9 is treated as the
postgresql family).
"""
PG_FAMILY = {"postgresql", "postgres", "postgresql9", "pg", "db9"}


def _applies(cond):
    for c in cond:
        p = c.split()
        if len(p) < 2:
            continue
        kind, db = p[0], p[1].lower()
        if kind == "onlyif" and db not in PG_FAMILY:
            return False
        if kind == "skipif" and db in PG_FAMILY:
            return False
    return True


def parse(text):
    lines = text.split("\n")
    i, n = 0, len(lines)
    records = []
    while i < n:
        s = lines[i].strip()
        if s == "" or s.startswith("#"):
            i += 1
            continue
        cond = []
        while i < n and (lines[i].startswith("skipif") or lines[i].startswith("onlyif")):
            cond.append(lines[i].strip())
            i += 1
        if i >= n:
            break
        line = lines[i]
        ctl_line = i + 1  # 1-based provenance
        if line.startswith("halt"):
            break
        if line.startswith("hash-threshold"):
            i += 1
            continue
        if line.startswith("statement"):
            parts = line.split()
            expect_ok = not (len(parts) > 1 and parts[1] == "error")
            i += 1
            sql = []
            while i < n and lines[i].strip() != "":
                sql.append(lines[i])
                i += 1
            if _applies(cond):
                records.append({"kind": "statement", "expect_ok": expect_ok,
                                "sql": "\n".join(sql).strip(), "line": ctl_line})
        elif line.startswith("query"):
            parts = line.split()
            types = parts[1] if len(parts) > 1 else ""
            sort = "nosort"
            if len(parts) > 2 and parts[2] in ("nosort", "rowsort", "valuesort"):
                sort = parts[2]
            i += 1
            sql = []
            while i < n and lines[i].strip() != "----":
                sql.append(lines[i])
                i += 1
            if i < n and lines[i].strip() == "----":   # consume expected block (ignored)
                i += 1
                while i < n and lines[i].strip() != "":
                    i += 1
            if _applies(cond):
                records.append({"kind": "query", "types": types, "sort": sort,
                                "sql": "\n".join(sql).strip(), "line": ctl_line})
        else:
            i += 1
    return records
