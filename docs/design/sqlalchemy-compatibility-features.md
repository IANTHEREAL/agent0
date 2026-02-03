# SQLAlchemy Compatibility Features - Design Document

**Date**: 2026-02-03  
**Author**: AI Assistant  
**Status**: Draft  
**Related**: `examples/01_python_todo_test/LIMITATIONS.md`

---

## Executive Summary

This document outlines the design for addressing pg-tikv limitations discovered during SQLAlchemy/psycopg2 compatibility testing. The features are prioritized by impact and effort.

| Feature | Priority | Effort | Status |
|---------|----------|--------|--------|
| F1: `pg_type_is_visible` function | P0 | 1 day | Not Started |
| F2: ARRAY protocol encoding fix | P1 | 1 week | Not Started |
| F3: GIN index for ARRAY types | P2 | 1 week | Not Started |
| F4: Better trigger error handling | P2 | 2 days | Not Started |
| F5: Full-Text Search (FTS) | P3 | 3-4 weeks | Not Started |
| F6: Enhanced PL/pgSQL support | P3 | 2-4 weeks | Not Started |

---

## F1: `pg_type_is_visible` Function

### Problem Statement

SQLAlchemy uses `pg_type_is_visible(oid)` to check if an enum type exists in the current search_path before creating or dropping it. This function is not implemented in pg-tikv, causing:

```
psycopg2.errors.InternalError_: Unsupported function: pg_type_is_visible

[SQL: SELECT pg_catalog.pg_type.typname
FROM pg_catalog.pg_type JOIN pg_catalog.pg_namespace ...
WHERE ... AND pg_catalog.pg_type_is_visible(pg_catalog.pg_type.oid) ...]
```

### Current State

- `pg_table_is_visible` IS implemented in `src/sql/expr/functions/pg_compat.rs:178`
- `pg_type_is_visible` is NOT registered

```rust
// Current implementation of pg_table_is_visible
pub fn pg_table_is_visible(_args: Vec<Value>) -> Result<Value> {
    Ok(Value::Boolean(true))
}
```

### Proposed Solution

Add `pg_type_is_visible` with the same behavior as `pg_table_is_visible`.

**Rationale**: In pg-tikv's keyspace-isolated architecture, all types within a tenant's keyspace are effectively "visible" since there's no cross-tenant schema leakage.

### Implementation Details

**File**: `src/sql/expr/functions/pg_compat.rs`

```rust
// Add to register() function
pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    // ... existing registrations ...
    map.insert("PG_TYPE_IS_VISIBLE", pg_type_is_visible);
}

/// Check if a type is visible in the current search_path.
/// 
/// In pg-tikv, all types within the keyspace are visible, so this always
/// returns true (similar to pg_table_is_visible).
/// 
/// PostgreSQL signature: pg_type_is_visible(type_oid oid) → boolean
pub fn pg_type_is_visible(_args: Vec<Value>) -> Result<Value> {
    Ok(Value::Boolean(true))
}
```

### Testing Plan

1. **Unit test** in `src/sql/expr/functions/pg_compat.rs`:
```rust
#[test]
fn test_pg_type_is_visible() {
    assert_eq!(
        pg_type_is_visible(vec![Value::Int32(12345)]).unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        pg_type_is_visible(vec![Value::Null]).unwrap(),
        Value::Boolean(true)
    );
}
```

2. **Integration test** - Add to `tests/`:
```sql
-- tests/XX_pg_type_is_visible.sql
SELECT pg_type_is_visible(23);  -- int4 OID
SELECT pg_type_is_visible(25);  -- text OID

-- Test with enum type
CREATE TYPE test_mood AS ENUM ('happy', 'sad');
SELECT pg_type_is_visible(t.oid) FROM pg_type t WHERE t.typname = 'test_mood';
DROP TYPE test_mood;
```

### Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| Always returning `true` may differ from PostgreSQL behavior | Document as intentional simplification; pg-tikv lacks multi-schema complexity |

### Estimated Effort

- Implementation: 30 minutes
- Testing: 1 hour
- Documentation: 30 minutes
- **Total**: ~2 hours

---

## F2: ARRAY Protocol Encoding Fix

### Problem Statement

PostgreSQL ARRAY columns return strings instead of Python lists in psycopg2:

```python
# Expected
todo.tags  # ['learning', 'database']

# Actual
todo.tags  # ['{', 'l', 'e', 'a', 'r', 'n', 'i', 'n', 'g', ',', ...]
# The string "{learning,database}" is being iterated as characters
```

### Current State

**Internal storage**: ✅ Correct
- `Value::Array(Vec<Value>)` properly stores arrays
- TiKV encoding/decoding works correctly

**Wire protocol**: ⚠️ Potential issue
- `src/protocol/handler.rs:5765-5788` encodes arrays as text format: `{item1,item2}`
- This is valid PostgreSQL text format

**Hypothesis**: The issue is likely one of:
1. Incorrect OID returned for array columns (client doesn't know it's an array)
2. Text encoding escaping issues causing parsing failures
3. Missing type information in result description

### Current Array Encoding

```rust
// src/protocol/handler.rs:5765-5786
fn encode_array(elems: &[Value]) -> String {
    let mut parts = Vec::with_capacity(elems.len());
    for elem in elems {
        let part = match elem {
            Value::Null => "NULL".to_string(),
            Value::Array(nested) => encode_array(nested),
            other => {
                let s = match other {
                    Value::Text(t) => t.clone(),
                    v => v.to_string(),
                };
                if needs_array_quotes(&s) {
                    format!("\"{}\"", escape_array_element(&s))
                } else {
                    s
                }
            }
        };
        parts.push(part);
    }
    format!("{{{}}}", parts.join(","))
}
```

### Investigation Required

Before implementing a fix, we need to investigate:

1. **OID assignment** - Check if `datatype_to_pgtype()` returns correct array OIDs:
   - `INT4ARRAY` = 1007
   - `TEXTARRAY` = 1009
   - `INT8ARRAY` = 1016

2. **Type inference** - Check if `infer_result_fields_from_query()` correctly identifies array columns

3. **Client-side parsing** - Verify psycopg2 behavior with:
   ```python
   # Test with direct PostgreSQL
   cursor.execute("SELECT ARRAY[1,2,3]")
   print(type(cursor.fetchone()[0]))  # Should be list
   
   # Compare wire-level protocol
   ```

### Proposed Solution

**Option A: Fix OID mapping** (Most likely fix)

```rust
// src/protocol/handler.rs - datatype_to_pgtype()
fn datatype_to_pgtype(dt: &DataType) -> Type {
    match dt {
        DataType::Array(elem_type) => {
            // Return proper array OID based on element type
            match elem_type.as_ref() {
                DataType::Int32 => Type::INT4_ARRAY,  // OID 1007
                DataType::Int64 => Type::INT8_ARRAY,  // OID 1016
                DataType::Text => Type::TEXT_ARRAY,   // OID 1009
                DataType::Boolean => Type::BOOL_ARRAY, // OID 1000
                DataType::Float64 => Type::FLOAT8_ARRAY, // OID 1022
                _ => Type::TEXT_ARRAY, // Default fallback
            }
        }
        // ... existing cases
    }
}
```

**Option B: Binary protocol encoding** (More complex, better performance)

Implement binary-format array encoding per PostgreSQL protocol spec:
- 4-byte: number of dimensions
- 4-byte: has-null flag
- 4-byte: element OID
- Per dimension: 4-byte length, 4-byte lower bound
- Element data with 4-byte length prefix each

### Implementation Details

#### Phase 1: Diagnosis (0.5 days)

1. Add debug logging to trace OIDs being sent
2. Compare with real PostgreSQL using `psql` debug mode
3. Test with different drivers (psycopg2, asyncpg, pg8000)

#### Phase 2: OID Fix (1 day)

Files to modify:
- `src/protocol/handler.rs` - `datatype_to_pgtype()`
- `src/sql/helpers.rs` - `infer_expr_type()` for array expressions

```rust
// Add array OID constants if not present
const INT4_ARRAY_OID: i32 = 1007;
const TEXT_ARRAY_OID: i32 = 1009;
const INT8_ARRAY_OID: i32 = 1016;
const BOOL_ARRAY_OID: i32 = 1000;
const FLOAT8_ARRAY_OID: i32 = 1022;
```

#### Phase 3: Text Format Verification (0.5 days)

Ensure text encoding matches PostgreSQL exactly:
- Empty array: `{}`
- NULL element: `{NULL}` (no quotes)
- Empty string element: `{""}` (quoted)
- String with special chars: `{"a,b","c\"d"}`

### Testing Plan

1. **Unit tests** for array encoding edge cases:
```rust
#[test]
fn test_encode_array_text_format() {
    // Basic integers
    let arr = vec![Value::Int32(1), Value::Int32(2)];
    assert_eq!(encode_array(&arr), "{1,2}");
    
    // Text with special chars
    let arr = vec![Value::Text("a,b".into()), Value::Text("c\"d".into())];
    assert_eq!(encode_array(&arr), r#"{"a,b","c\"d"}"#);
    
    // Nested arrays
    let arr = vec![
        Value::Array(vec![Value::Int32(1), Value::Int32(2)]),
        Value::Array(vec![Value::Int32(3), Value::Int32(4)]),
    ];
    assert_eq!(encode_array(&arr), "{{1,2},{3,4}}");
}
```

2. **Integration test** with psycopg2:
```python
# tests/python/test_array_types.py
def test_array_returns_list():
    cursor.execute("SELECT ARRAY[1, 2, 3]")
    result = cursor.fetchone()[0]
    assert isinstance(result, list)
    assert result == [1, 2, 3]

def test_text_array_returns_list():
    cursor.execute("SELECT ARRAY['a', 'b', 'c']")
    result = cursor.fetchone()[0]
    assert isinstance(result, list)
    assert result == ['a', 'b', 'c']
```

3. **ORM test** - Add to `orm-tests/`:
```typescript
// Test SQLAlchemy-style array operations
const result = await db.query("SELECT tags FROM items WHERE id = 1");
expect(Array.isArray(result[0].tags)).toBe(true);
```

### Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| Binary encoding complexity | Start with OID fix; binary is optional optimization |
| Nested array edge cases | Comprehensive test suite with PostgreSQL comparison |
| Breaking existing clients | OID fix should be transparent; run full ORM test suite |

### Estimated Effort

- Investigation: 0.5 days
- Implementation: 1-2 days
- Testing: 1-2 days
- **Total**: 3-5 days

---

## F3: GIN Index for ARRAY Types

### Problem Statement

GIN indexes are created without error for ARRAY columns, but:
1. They may not accelerate ARRAY containment queries (`@>`, `<@`)
2. Only JSONB GIN is implemented

### Current State

**JSONB GIN**: ✅ Fully implemented
- `src/sql/gin.rs` - Token extraction for JSONB
- `src/sql/planner.rs` - GIN access path selection
- `src/sql/dml.rs` - GIN index maintenance on INSERT/UPDATE/DELETE

**ARRAY GIN**: ❌ Not implemented
- Index creation accepted but not used
- No token extraction for arrays
- No query planner support

### Proposed Solution

Extend the existing GIN infrastructure to support ARRAY types.

### Implementation Details

#### Phase 1: ARRAY Token Extraction (2 days)

**File**: `src/sql/gin.rs`

```rust
/// Extract GIN tokens from an array value.
/// Each array element becomes a token.
pub(crate) fn extract_array_gin_tokens(arr: &[Value]) -> Vec<u64> {
    let mut tokens = Vec::with_capacity(arr.len());
    for elem in arr {
        tokens.push(hash_array_element(elem));
    }
    tokens.sort_unstable();
    tokens.dedup();
    tokens
}

fn hash_array_element(value: &Value) -> u64 {
    let mut h = FNV1A_OFFSET_BASIS;
    h = fnv1a_u64(h, b"ARRAY_ELEM:");
    match value {
        Value::Null => fnv1a_u64(h, b"NULL"),
        Value::Int32(n) => fnv1a_u64(h, &n.to_be_bytes()),
        Value::Int64(n) => fnv1a_u64(h, &n.to_be_bytes()),
        Value::Text(s) => fnv1a_u64(h, s.as_bytes()),
        Value::Boolean(b) => fnv1a_u64(h, if *b { b"T" } else { b"F" }),
        // ... other types
        _ => fnv1a_u64(h, &format!("{:?}", value).as_bytes()),
    }
}
```

#### Phase 2: Query Planning (2 days)

**File**: `src/sql/planner.rs`

Add recognition for array containment operators:

```rust
fn choose_gin_access_path(...) -> Option<AccessPath> {
    // Existing JSONB @> check
    if is_jsonb_contains(filter_expr, schema) {
        return Some(plan_jsonb_gin_scan(...));
    }
    
    // NEW: ARRAY @> check
    if is_array_contains(filter_expr, schema) {
        return Some(plan_array_gin_scan(...));
    }
    
    None
}

fn is_array_contains(expr: &Expr, schema: &TableSchema) -> Option<(usize, &[Value])> {
    // Match: column @> ARRAY[...] or ARRAY[...] <@ column
    match expr {
        Expr::BinaryOp { left, op: BinaryOperator::Contains, right } => {
            // Check if left is array column and right is array literal
            ...
        }
        _ => None,
    }
}
```

#### Phase 3: Index Maintenance (1 day)

**File**: `src/sql/dml.rs`

Extend `extract_gin_token_hashes_from_row` to handle arrays:

```rust
fn extract_gin_token_hashes_from_row(
    schema: &TableSchema,
    index: &IndexDef,
    row: &Row,
) -> Result<Vec<u64>> {
    let col_idx = supported_gin_index_column(schema, index)?;
    let value = &row.values[col_idx];
    
    match value {
        Value::Jsonb(s) | Value::Json(s) => {
            // Existing JSONB logic
            let json = serde_json::from_str(s)?;
            Ok(gin::extract_gin_tokens(&json).into_scan_hashes())
        }
        Value::Array(arr) => {
            // NEW: Array logic
            Ok(gin::extract_array_gin_tokens(arr))
        }
        _ => Err(anyhow!("GIN index requires JSONB or ARRAY column")),
    }
}
```

### Testing Plan

1. **Unit tests** for array token extraction:
```rust
#[test]
fn test_extract_array_gin_tokens() {
    let arr = vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)];
    let tokens = extract_array_gin_tokens(&arr);
    assert_eq!(tokens.len(), 3);
    
    // Same elements should produce same tokens
    let arr2 = vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)];
    let tokens2 = extract_array_gin_tokens(&arr2);
    assert_eq!(tokens, tokens2);
}
```

2. **Integration test**:
```sql
-- tests/XX_gin_array.sql
CREATE TABLE items (
    id SERIAL PRIMARY KEY,
    tags TEXT[]
);

CREATE INDEX idx_tags_gin ON items USING GIN (tags);

INSERT INTO items (tags) VALUES (ARRAY['rust', 'database']);
INSERT INTO items (tags) VALUES (ARRAY['python', 'web']);
INSERT INTO items (tags) VALUES (ARRAY['rust', 'web']);

-- Should use GIN index
EXPLAIN SELECT * FROM items WHERE tags @> ARRAY['rust'];
SELECT * FROM items WHERE tags @> ARRAY['rust'] ORDER BY id;

DROP TABLE items;
```

### Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| Hash collisions for array elements | Use high-quality FNV-1a hash; collisions cause false positives, not correctness issues |
| Large arrays cause write amplification | Document maximum recommended array size; same tradeoff as JSONB |

### Estimated Effort

- Token extraction: 2 days
- Query planning: 2 days
- Index maintenance: 1 day
- Testing: 1-2 days
- **Total**: 6-8 days

---

## F4: Better Trigger Error Handling

### Problem Statement

Triggers using unsupported functions (like `to_tsvector()`) fail silently or with cryptic errors. Users don't know which specific function is unsupported.

### Current State

Trigger body is parsed line-by-line (`src/sql/triggers.rs:130`):
```rust
for line in block_content.lines() {
    // ... process line ...
    // If a function fails, error bubbles up without context
}
```

### Proposed Solution

Add validation phase before execution that checks for unsupported functions and provides clear error messages.

### Implementation Details

**File**: `src/sql/triggers.rs`

```rust
/// Validate trigger body before execution, checking for unsupported functions.
fn validate_trigger_body(body: &str) -> Result<()> {
    let unsupported_functions = [
        "to_tsvector",
        "plainto_tsquery", 
        "to_tsquery",
        "ts_rank",
        "ts_rank_cd",
        "setweight",
        "tsvector_update_trigger",
    ];
    
    let body_lower = body.to_lowercase();
    
    for func in unsupported_functions {
        if body_lower.contains(func) {
            return Err(anyhow!(
                "Trigger function uses unsupported function '{}'. \
                 Full-text search functions are not yet implemented in pg-tikv. \
                 See: https://github.com/pg-tikv/pg-tikv/issues/XXX",
                func
            ));
        }
    }
    
    Ok(())
}

// Call at start of execute_trigger_body
async fn execute_trigger_body(...) -> Result<TriggerResult> {
    validate_trigger_body(body)?;
    // ... rest of implementation
}
```

### Testing Plan

```sql
-- tests/XX_trigger_error_messages.sql
CREATE TABLE test_fts (id SERIAL, content TEXT, search_vector TEXT);

CREATE OR REPLACE FUNCTION update_search_vector()
RETURNS trigger AS $$
BEGIN
    NEW.search_vector := to_tsvector('english', NEW.content);
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER test_trigger
BEFORE INSERT ON test_fts
FOR EACH ROW EXECUTE FUNCTION update_search_vector();

-- Should fail with clear error message
INSERT INTO test_fts (content) VALUES ('test');
-- Expected: "Trigger function uses unsupported function 'to_tsvector'..."

DROP TABLE test_fts CASCADE;
DROP FUNCTION update_search_vector();
```

### Estimated Effort

- Implementation: 2-3 hours
- Testing: 1-2 hours
- **Total**: 4-5 hours

---

## F5: Full-Text Search (FTS)

### Problem Statement

PostgreSQL full-text search features are not implemented:
- `TSVECTOR` data type
- `to_tsvector()`, `plainto_tsquery()`, `to_tsquery()` functions
- `@@` match operator
- `ts_rank()` ranking function

### Scope

This is a large feature. This design covers MVP implementation.

### MVP Requirements

1. `TSVECTOR` type that stores text tokens
2. `to_tsvector(config, text)` - tokenize text (basic whitespace tokenization)
3. `plainto_tsquery(config, text)` - create query from plain text
4. `@@` operator - check if tsvector matches tsquery
5. `ts_rank(tsvector, tsquery)` - basic relevance score

**Out of scope for MVP**:
- Language-specific stemming (use simple tokenization)
- Multiple language configs (accept but ignore config parameter)
- `ts_headline()` result highlighting
- `setweight()` weighted vectors
- Phrase search (`<->` operator)

### Data Model

**Option A: String-based (Simpler)**
```rust
// Store as sorted, space-delimited tokens
Value::Tsvector(String)  // e.g., "'database' 'postgresql' 'search'"
```

**Option B: Structured (Better performance)**
```rust
Value::Tsvector(Vec<Token>)

struct Token {
    lexeme: String,
    positions: Vec<u16>,  // For phrase search (future)
    weight: u8,           // For setweight (future)
}
```

**Recommendation**: Start with Option A for MVP, migrate to Option B if needed.

### Implementation Details

#### Phase 1: Type System (2 days)

**File**: `src/types/mod.rs`

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DataType {
    // ... existing types ...
    Tsvector,
    Tsquery,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    // ... existing values ...
    Tsvector(String),  // Normalized token string
    Tsquery(String),   // Query string
}
```

**File**: `src/protocol/handler.rs`

```rust
fn datatype_to_pgtype(dt: &DataType) -> Type {
    match dt {
        DataType::Tsvector => Type::new("tsvector".into(), 3614),
        DataType::Tsquery => Type::new("tsquery".into(), 3615),
        // ...
    }
}
```

#### Phase 2: Functions (3 days)

**File**: `src/sql/expr/functions/fts.rs` (new file)

```rust
use crate::types::Value;
use anyhow::Result;

/// Tokenize text into tsvector.
/// 
/// MVP: Simple whitespace tokenization, lowercase, remove punctuation.
/// Future: Language-aware stemming via rust-stemmers crate.
pub fn to_tsvector(args: Vec<Value>) -> Result<Value> {
    let text = match args.len() {
        1 => extract_text(&args[0])?,
        2 => extract_text(&args[1])?, // Ignore config
        _ => return Err(anyhow!("to_tsvector requires 1 or 2 arguments")),
    };
    
    let tokens = tokenize(&text);
    let normalized = tokens.join(" ");
    Ok(Value::Tsvector(normalized))
}

fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty() && s.len() >= 2)
        .map(String::from)
        .collect::<std::collections::BTreeSet<_>>() // Dedupe and sort
        .into_iter()
        .collect()
}

/// Create tsquery from plain text.
pub fn plainto_tsquery(args: Vec<Value>) -> Result<Value> {
    let text = match args.len() {
        1 => extract_text(&args[0])?,
        2 => extract_text(&args[1])?,
        _ => return Err(anyhow!("plainto_tsquery requires 1 or 2 arguments")),
    };
    
    let tokens = tokenize(&text);
    // Join with & for AND semantics
    let query = tokens.join(" & ");
    Ok(Value::Tsquery(query))
}

/// Match tsvector against tsquery.
pub fn ts_match(tsvector: &Value, tsquery: &Value) -> Result<Value> {
    let (Value::Tsvector(vec_str), Value::Tsquery(query_str)) = (tsvector, tsquery) else {
        return Err(anyhow!("@@ requires tsvector and tsquery operands"));
    };
    
    let vec_tokens: std::collections::HashSet<&str> = vec_str.split_whitespace().collect();
    
    // Simple AND matching
    let query_tokens: Vec<&str> = query_str
        .split(" & ")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    
    let matches = query_tokens.iter().all(|t| vec_tokens.contains(t));
    Ok(Value::Boolean(matches))
}

/// Rank tsvector against tsquery (basic implementation).
pub fn ts_rank(args: Vec<Value>) -> Result<Value> {
    // MVP: Return ratio of matching tokens
    let (tsvector, tsquery) = match args.as_slice() {
        [tv, tq] => (tv, tq),
        [_weights, tv, tq] => (tv, tq), // Ignore weights
        [_weights, tv, tq, _norm] => (tv, tq), // Ignore weights and normalization
        _ => return Err(anyhow!("ts_rank requires 2-4 arguments")),
    };
    
    let Value::Tsvector(vec_str) = tsvector else {
        return Err(anyhow!("ts_rank first argument must be tsvector"));
    };
    let Value::Tsquery(query_str) = tsquery else {
        return Err(anyhow!("ts_rank second argument must be tsquery"));
    };
    
    let vec_tokens: std::collections::HashSet<&str> = vec_str.split_whitespace().collect();
    let query_tokens: Vec<&str> = query_str.split(" & ").map(|s| s.trim()).collect();
    
    if query_tokens.is_empty() {
        return Ok(Value::Float64(0.0));
    }
    
    let matches = query_tokens.iter().filter(|t| vec_tokens.contains(*t)).count();
    let rank = matches as f64 / query_tokens.len() as f64;
    
    Ok(Value::Float64(rank))
}
```

#### Phase 3: Operator (1 day)

**File**: `src/sql/expr/operators.rs`

```rust
// Add to eval_binary_op
BinaryOperator::Custom(op) if op == "@@" => {
    match (&left, &right) {
        (Value::Tsvector(_), Value::Tsquery(_)) => fts::ts_match(&left, &right),
        (Value::Tsquery(_), Value::Tsvector(_)) => fts::ts_match(&right, &left),
        _ => Err(anyhow!("@@ requires tsvector and tsquery operands")),
    }
}
```

#### Phase 4: GIN Integration (1-2 weeks, separate feature)

See F3 design, extended for TSVECTOR.

### Testing Plan

```sql
-- tests/XX_fts_basic.sql

-- Basic tokenization
SELECT to_tsvector('The quick brown fox');
-- Expected: 'brown' 'fox' 'quick' 'the'

-- Query creation
SELECT plainto_tsquery('quick fox');
-- Expected: 'quick' & 'fox'

-- Matching
SELECT to_tsvector('The quick brown fox') @@ plainto_tsquery('quick fox');
-- Expected: true

SELECT to_tsvector('The quick brown fox') @@ plainto_tsquery('slow turtle');
-- Expected: false

-- Ranking
SELECT ts_rank(to_tsvector('The quick brown fox'), plainto_tsquery('quick fox'));
-- Expected: 1.0 (both tokens match)

SELECT ts_rank(to_tsvector('The quick brown fox'), plainto_tsquery('quick turtle'));
-- Expected: 0.5 (one of two tokens match)

-- Table usage
CREATE TABLE documents (
    id SERIAL PRIMARY KEY,
    title TEXT,
    body TEXT,
    search_vector TSVECTOR
);

INSERT INTO documents (title, body, search_vector) 
VALUES ('PostgreSQL Guide', 'Learn about PostgreSQL database', 
        to_tsvector('PostgreSQL Guide Learn about PostgreSQL database'));

SELECT * FROM documents 
WHERE search_vector @@ plainto_tsquery('postgresql database')
ORDER BY ts_rank(search_vector, plainto_tsquery('postgresql database')) DESC;

DROP TABLE documents;
```

### Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| Performance without GIN | Document that GIN is needed for large tables |
| Stemming quality | Start simple; add rust-stemmers later |
| Feature creep | MVP scope is strict; phrase search etc. are separate features |

### Estimated Effort

- Type system: 2 days
- Functions: 3 days
- Operator: 1 day
- Testing: 2-3 days
- Documentation: 1 day
- **Total MVP**: 9-12 days (3-4 weeks with reviews and iteration)

---

## F6: Enhanced PL/pgSQL Support

### Problem Statement

Current PL/pgSQL support is minimal:
- Only simple assignments (`NEW.col := expr`)
- Only `RETURN NEW/OLD/NULL`
- No control flow (IF/WHILE)
- No local variables
- No exception handling

### Current State

**File**: `src/sql/triggers.rs` - Line-by-line execution
**File**: `src/sql/plpgsql.rs` - Basic parsing

### MVP Scope

1. IF/ELSIF/ELSE/END IF
2. Simple LOOP with EXIT
3. DECLARE block with local variables
4. RAISE NOTICE/WARNING/EXCEPTION

**Out of scope**:
- FOR loops
- WHILE loops
- RETURN QUERY
- Exception handlers (BEGIN/EXCEPTION/END)
- Cursors

### Implementation Details

#### Architecture

```
┌─────────────────┐
│  PL/pgSQL Text  │
└────────┬────────┘
         │ parse
         ▼
┌─────────────────┐
│   AST Nodes     │
│  (PlpgsqlStmt)  │
└────────┬────────┘
         │ execute
         ▼
┌─────────────────┐
│  Interpreter    │
│  (with scope)   │
└─────────────────┘
```

**File**: `src/sql/plpgsql.rs`

```rust
#[derive(Debug, Clone)]
pub enum PlpgsqlStmt {
    Assignment { target: String, expr: String },
    Return { expr: Option<String> },
    If {
        condition: String,
        then_stmts: Vec<PlpgsqlStmt>,
        elsif_branches: Vec<(String, Vec<PlpgsqlStmt>)>,
        else_stmts: Vec<PlpgsqlStmt>,
    },
    Loop {
        label: Option<String>,
        body: Vec<PlpgsqlStmt>,
    },
    Exit { label: Option<String>, when: Option<String> },
    Raise { level: String, message: String, params: Vec<String> },
    Sql { statement: String },
}

pub struct PlpgsqlScope {
    variables: HashMap<String, Value>,
    parent: Option<Box<PlpgsqlScope>>,
}

impl PlpgsqlScope {
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.variables.get(name)
            .or_else(|| self.parent.as_ref().and_then(|p| p.get(name)))
    }
    
    pub fn set(&mut self, name: &str, value: Value) {
        self.variables.insert(name.to_string(), value);
    }
}
```

### Example: IF Statement Execution

```rust
async fn execute_if(
    &self,
    condition: &str,
    then_stmts: &[PlpgsqlStmt],
    elsif_branches: &[(String, Vec<PlpgsqlStmt>)],
    else_stmts: &[PlpgsqlStmt],
    scope: &mut PlpgsqlScope,
) -> Result<Option<PlpgsqlResult>> {
    // Evaluate condition
    let cond_value = self.eval_expression(condition, scope).await?;
    
    if cond_value.as_bool()? {
        return self.execute_statements(then_stmts, scope).await;
    }
    
    for (elsif_cond, elsif_stmts) in elsif_branches {
        let elsif_value = self.eval_expression(elsif_cond, scope).await?;
        if elsif_value.as_bool()? {
            return self.execute_statements(elsif_stmts, scope).await;
        }
    }
    
    if !else_stmts.is_empty() {
        return self.execute_statements(else_stmts, scope).await;
    }
    
    Ok(None)
}
```

### Testing Plan

```sql
-- tests/XX_plpgsql_if.sql
CREATE OR REPLACE FUNCTION test_if(x INT) RETURNS TEXT AS $$
DECLARE
    result TEXT;
BEGIN
    IF x > 10 THEN
        result := 'large';
    ELSIF x > 5 THEN
        result := 'medium';
    ELSE
        result := 'small';
    END IF;
    RETURN result;
END;
$$ LANGUAGE plpgsql;

SELECT test_if(15);  -- 'large'
SELECT test_if(7);   -- 'medium'
SELECT test_if(3);   -- 'small'

DROP FUNCTION test_if;
```

### Estimated Effort

- Parser improvements: 1 week
- IF/ELSIF/ELSE: 3 days
- LOOP/EXIT: 2 days
- DECLARE/variables: 3 days
- RAISE: 1 day
- Testing: 1 week
- **Total**: 2-4 weeks

---

## Appendix: File Change Summary

| Feature | Files to Modify | New Files |
|---------|----------------|-----------|
| F1: pg_type_is_visible | `src/sql/expr/functions/pg_compat.rs` | - |
| F2: ARRAY protocol | `src/protocol/handler.rs` | - |
| F3: GIN for ARRAY | `src/sql/gin.rs`, `src/sql/planner.rs`, `src/sql/dml.rs` | - |
| F4: Trigger errors | `src/sql/triggers.rs` | - |
| F5: FTS | `src/types/mod.rs`, `src/protocol/handler.rs`, `src/sql/expr/operators.rs` | `src/sql/expr/functions/fts.rs` |
| F6: PL/pgSQL | `src/sql/plpgsql.rs`, `src/sql/triggers.rs` | - |

---

## References

- PostgreSQL Documentation: [Full Text Search](https://www.postgresql.org/docs/current/textsearch.html)
- PostgreSQL Documentation: [GIN Indexes](https://www.postgresql.org/docs/current/gin.html)
- PostgreSQL Documentation: [PL/pgSQL](https://www.postgresql.org/docs/current/plpgsql.html)
- pgwire Protocol: [Message Formats](https://www.postgresql.org/docs/current/protocol-message-formats.html)
