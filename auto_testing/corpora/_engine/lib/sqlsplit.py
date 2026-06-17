"""
PostgreSQL regress-file statement splitter.

Splits a .sql file (psql input) into an ordered list of items. Each item is
either a SQL statement or a psql backslash meta-command. The splitter is a
character-level state machine that understands the lexical constructs PG's
regress corpus actually uses, so a stray ';' inside a string/comment/dollar
body does not prematurely terminate a statement.

Handled:
  - line comments  -- ... \n
  - block comments /* ... */   (PG allows nesting)
  - standard strings 'a''b'     ('' escapes a quote; backslash is literal)
  - escape strings  E'a\\'b'    (backslash escapes inside)
  - quoted idents   "a""b"      ("" escapes)
  - dollar quoting  $$ .. $$ / $tag$ .. $tag$
  - psql meta-commands: a line whose first non-space char is '\' (consumed to EOL)

This is intentionally NOT a full SQL parser. It only needs to find correct
statement boundaries. Anything subtler is left to the two real servers.
"""

from dataclasses import dataclass


@dataclass
class Item:
    kind: str          # "sql" | "meta"
    text: str          # the statement text (sql: without trailing ';'); meta: full "\..." line
    line: int          # 1-based line number where the item starts


def _is_ident_char(c: str) -> bool:
    return c.isalnum() or c == "_"


def split_sql(src: str):
    items = []
    i, n = 0, len(src)
    line = 1
    start_line = 1
    buf = []

    def at_stmt_start():
        # True if buf so far is only whitespace (we're between statements)
        return "".join(buf).strip() == ""

    while i < n:
        c = src[i]

        # ---- psql meta-command: '\' as the first non-space token on a line ----
        if c == "\\" and at_stmt_start():
            # consume to end of line
            j = i
            while j < n and src[j] != "\n":
                j += 1
            meta = src[i:j]
            items.append(Item("meta", meta.strip(), line))
            # advance, account for newline
            if j < n:  # the '\n'
                line += 1
                j += 1
            i = j
            buf = []
            start_line = line
            continue

        # ---- line comment ----
        if c == "-" and i + 1 < n and src[i + 1] == "-":
            buf.append(c)
            i += 1
            while i < n and src[i] != "\n":
                buf.append(src[i])
                i += 1
            continue

        # ---- block comment (nesting) ----
        if c == "/" and i + 1 < n and src[i + 1] == "*":
            depth = 1
            buf.append(src[i]); buf.append(src[i + 1])
            i += 2
            while i < n and depth > 0:
                if src[i] == "\n":
                    line += 1
                if src[i] == "/" and i + 1 < n and src[i + 1] == "*":
                    depth += 1; buf.append(src[i]); buf.append(src[i + 1]); i += 2; continue
                if src[i] == "*" and i + 1 < n and src[i + 1] == "/":
                    depth -= 1; buf.append(src[i]); buf.append(src[i + 1]); i += 2; continue
                buf.append(src[i]); i += 1
            continue

        # ---- quoted identifier "..." ----
        if c == '"':
            buf.append(c); i += 1
            while i < n:
                if src[i] == "\n":
                    line += 1
                if src[i] == '"':
                    if i + 1 < n and src[i + 1] == '"':
                        buf.append('"'); buf.append('"'); i += 2; continue
                    buf.append('"'); i += 1; break
                buf.append(src[i]); i += 1
            continue

        # ---- string literal: distinguish escape-string E'...' from standard '...' ----
        if c == "'":
            # is the immediately preceding char an 'e'/'E' escape-string marker?
            prev = src[i - 1] if i > 0 else ""
            prev2 = src[i - 2] if i > 1 else ""
            is_escape = prev in ("e", "E") and not _is_ident_char(prev2)
            buf.append(c); i += 1
            while i < n:
                ch = src[i]
                if ch == "\n":
                    line += 1
                if is_escape and ch == "\\" and i + 1 < n:
                    buf.append(ch); buf.append(src[i + 1]); i += 2; continue
                if ch == "'":
                    if i + 1 < n and src[i + 1] == "'":
                        buf.append("'"); buf.append("'"); i += 2; continue
                    buf.append("'"); i += 1; break
                buf.append(ch); i += 1
            continue

        # ---- dollar quoting $tag$ ... $tag$ ----
        if c == "$":
            # try to read a tag: $ [ident-chars]* $
            j = i + 1
            while j < n and (_is_ident_char(src[j])):
                j += 1
            if j < n and src[j] == "$":
                tag = src[i:j + 1]                  # includes both $...$
                buf.append(tag)
                i = j + 1
                # scan until matching tag
                while i < n:
                    if src[i] == "\n":
                        line += 1
                    if src[i] == "$" and src.startswith(tag, i):
                        buf.append(tag); i += len(tag); break
                    buf.append(src[i]); i += 1
                continue
            # not a dollar-quote start, treat '$' literally
            buf.append(c); i += 1
            continue

        # ---- statement terminator ----
        if c == ";":
            stmt = "".join(buf).strip()
            if stmt:
                items.append(Item("sql", stmt, start_line))
            i += 1
            buf = []
            start_line = line
            continue

        # ---- newline bookkeeping ----
        if c == "\n":
            line += 1

        buf.append(c)
        i += 1

    # trailing statement without terminator
    tail = "".join(buf).strip()
    if tail:
        items.append(Item("sql", tail, start_line))
    return items


if __name__ == "__main__":
    import sys
    with open(sys.argv[1]) as f:
        src = f.read()
    items = split_sql(src)
    sql = sum(1 for it in items if it.kind == "sql")
    meta = sum(1 for it in items if it.kind == "meta")
    print(f"{sys.argv[1]}: {len(items)} items ({sql} sql, {meta} meta)")
    for it in items[:8]:
        head = it.text.replace("\n", " ")[:90]
        print(f"  L{it.line:<5} {it.kind:<4} {head}")
