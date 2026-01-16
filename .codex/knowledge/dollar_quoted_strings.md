# Dollar-Quoted Strings (`$$...$$` / `$tag$...$tag$`): facts + code locations

## Design doc
- `docs/design/07_dollar_quoted_strings.md`
  - MVP requires:
    - Allow dollar-quoted strings in SQL and parse them as string constants.
    - Fix pgwire helper scanners so `$tag$...$tag$` regions are treated like string literals for:
      - placeholder counting (`$1`, `$2`, ...)
      - keyword search (e.g. `RETURNING`)

## sqlparser AST representation
- `sqlparser` crate v0.40.0:
  - `sqlparser::ast::Value::DollarQuotedString(DollarQuotedString)`
  - `sqlparser::ast::value::DollarQuotedString { value: String, tag: Option<String> }`

## Existing code paths affected

### Parse-error “unsupported” filter
- `src/sql/executor.rs`
  - `Executor::execute()` calls `crate::sql::parse_sql(sql)`.
  - If parsing errors, it consults `src/sql/helpers.rs::get_unsupported_reason(sql_upper)` to decide whether to return `ExecuteResult::Skipped`.
- `src/sql/helpers.rs`
  - `get_unsupported_reason(sql_upper: &str)` no longer treats `$$`/`$tag$` as unsupported (dollar-quoted strings are expected to be parsed by sqlparser).

### Expression evaluation (runtime)
- `src/sql/expr.rs`
  - `eval_value(v: &SqlValue)` handles:
    - `Null/Boolean/Number/SingleQuotedString/DoubleQuotedString`
    - `DollarQuotedString` (`Value::Text` from `DollarQuotedString.value`)

### pgwire placeholder/keyword scanning (protocol)
- `src/protocol/handler.rs`
  - `count_sql_parameters(sql)`:
    - Counts `$<digits>` outside `'...'`, `"..."`, and `$tag$...$tag$` / `$$...$$`.
  - `find_keyword_outside_strings(query, keyword)`:
    - Scans keyword outside `'...'`, `"..."`, and dollar-quoted strings (with identifier-boundary checks).
  - `substitute_parameters(query, portal)`:
    - Builds parameter literal strings, then substitutes `$n` placeholders only outside `'...'`, `"..."`, and dollar-quoted regions via `substitute_placeholders_outside_strings_and_dollar(...)`.
  - `replace_placeholders_for_inference(query)`:
    - Already tracks `$tag$...$tag$` / `$$...$$` regions via `dollar_delim` state and preserves placeholders inside them.
    - Unit tests exist at end of `src/protocol/handler.rs`:
      - `test_replace_placeholders_preserves_dollar_quoted_strings()`
  - Unit tests added (end of `src/protocol/handler.rs`):
    - `test_count_sql_parameters_ignores_dollar_quoted_strings()`
    - `test_find_keyword_outside_strings_ignores_dollar_quoted_strings()`
    - `test_substitute_placeholders_preserves_dollar_quoted_strings()`

## Related implementation (already supports dollar quotes elsewhere)
- `src/sql/executor_functions_triggers.rs`
  - `parse_sql_string_or_dollar_literal(s)`:
    - Extracts body from `$tag$...$tag$` or `$$...$$` (also supports `'...'`).
  - `find_keyword_outside_quotes_and_dollar(haystack, keyword)`:
    - Keyword scan that ignores `'...'`, `"..."`, and `$tag$...$tag$` regions.

## Existing tests already using dollar quotes
- `tests/46_functions_triggers_ddl.sql`
  - Defines PL/pgSQL trigger function using `AS $$ ... $$`.

## Dollar-quote integration coverage
- `tests/42_dollar_quote.sql`
  - Inserts/selects `$$hello$$` and `$tag$world$tag$` as text literals.
