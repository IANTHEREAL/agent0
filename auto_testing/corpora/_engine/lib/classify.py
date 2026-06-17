"""
Lean failure classifier shared by the differential engine.

Maps a db9 (sqlstate, message, sql) gap to a stable *feature-point id* string,
so the baseline can tag known gaps and link them to tracking issues. This is the
productised subset of the prototype's make_cases.classify().
"""
import re

BUILTIN_TYPES = {
    "money", "point", "line", "lseg", "box", "path", "polygon", "circle",
    "bit", "varbit", "bit varying", "b",
    "int4range", "int8range", "numrange", "daterange", "tsrange", "tstzrange",
    "int4multirange", "int8multirange", "nummultirange", "datemultirange",
    "tsmultirange", "tstzmultirange", "xml", "tsvector", "tsquery", "jsonpath",
    "cidr", "inet", "macaddr", "macaddr8", "pg_lsn", "pg_snapshot",
    "txid_snapshot", "regclass", "regproc", "regtype", "regrole", "regnamespace",
}


def feature_id(sqlstate, msg, sql):
    st = sqlstate or ""
    ml = (msg or "").lower()
    sl = " ".join((sql or "").lower().split())

    if (st == "XX000" and "parse error" in ml) or st == "42601":
        if re.search(r"\bpartition\s+(by|of)\b", sl) or "attach partition" in sl or "detach partition" in sl:
            return "ddl-partitioning"
        if re.search(r"\binherits\b", sl): return "ddl-table-inheritance"
        if "create statistics" in sl: return "ddl-extended-statistics"
        if re.search(r"create\s+(or\s+replace\s+)?rule\b", sl): return "ddl-rules"
        if re.search(r"create\s+(or\s+replace\s+)?operator\b", sl) or "operator class" in sl: return "ddl-custom-operators"
        if "foreign data wrapper" in sl or "foreign table" in sl or "create server" in sl: return "ddl-foreign-data"
        if "publication" in sl: return "repl-publication"
        if "subscription" in sl: return "repl-subscription"
        if "merge into" in sl: return "dml-merge"
        if re.search(r"explain\s*\(", sl): return "query-explain-options"
        if "check option" in sl: return "ddl-view-check-option"
        if re.search(r"create\s+type\s+\w+\s*\(", sl): return "ddl-base-type"
        if re.search(r"alter\s+table.*\bset\s*\(", sl) or re.search(r"with\s*\(\s*\w+\s*=", sl): return "ddl-storage-reloptions"
        if "xmltable" in sl or re.search(r"\bpassing\b", sl): return "query-xmltable"
        if "@?" in (sql or "") or "@@" in (sql or "") or "jsonpath" in sl: return "json-path-operators"
        if re.search(r"&<|&>|-\|-|<@|@>|\b\w*range\b", sl): return "types-range-operators"
        if "on conflict" in sl: return "dml-on-conflict"
        if re.search(r"create\s+(or\s+replace\s+)?aggregate", sl): return "ddl-create-aggregate"
        return "parser-other-unclassified"

    m = re.search(r'type "?([a-z0-9_ ]+?)"? does not exist', ml)
    if st in ("0A000", "42704") and m:
        t = m.group(1).strip()
        if t == "b" or re.search(r"\bb'", (sql or "").lower()): return "type-bit"
        if t in BUILTIN_TYPES: return f"type-{re.sub(r'[^a-z0-9]+','_',t)}"
        return "cascade-downstream"

    if st == "0A000" and "unknown function" in ml: return "fn-missing"
    if st == "0A000" and "table-valued" in ml: return "fn-table-valued"
    if st == "42883" and "operator does not exist" in ml: return "types-operator-resolution"
    if st == "42883" and "function" in ml: return "fn-missing"
    if st == "0A000" and "unsupported extract field" in ml: return "fn-extract-fields"
    if st == "0A000" and "modulo" in ml: return "types-modulo-coverage"
    if st == "0A000" and "collation" in ml: return "feat-collation"
    if st == "0A000" and "domain" in ml: return "feat-domain"
    if st == "0A000": return "feature-unsupported-other"

    if st == "22P02":
        t = re.search(r'invalid input syntax for type "?([a-z0-9_ ]+?)"?:', ml)
        return f"input-syntax-{re.sub(r'[^a-z0-9]+','_',(t.group(1).strip() if t else 'value'))}"
    if st == "XX000" and ml.strip() == "no pk": return "dml-update-delete-requires-pk"
    if st == "XX000" and "cannot compare" in ml: return "analyzer-typed-literal-type-loss"
    if "table functions only support language sql" in ml: return "plpgsql-setof-language"
    if "create function requires" in ml: return "plpgsql-sql-body-functions"
    if "set-returning function, not supported in this con" in ml: return "query-srf-in-expression"
    if "to_char cannot format" in ml: return "fn-to_char-formatting"
    if st == "XX000" and ("does not exist" in ml or "already exists" in ml): return "errors-nonstandard-sqlstate"
    if st in ("42P01", "42703", "42704", "42P02", "3F000", "42P06", "42P07"): return "cascade-downstream"
    return "other-divergence"
