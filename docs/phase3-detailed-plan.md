# Phase 3 — FTS Function Enhancement: Detailed Implementation Plan

> **Goal**: Implement `phraseto_tsquery`, `websearch_to_tsquery`, `ts_headline`, and improved `ts_rank`/`ts_rank_cd`.
> **Principle**: PostgreSQL 17 is the specification. All behavior must match PG exactly.
> **Estimated effort**: 5-6 days
> **Review status**: Round 3 FINAL (Metis analysis + self-review addressing MatchResult, thin wrapper, CJK offsets)

---

## Task 0: Pre-existing Bug Fixes (MUST do before ANY Phase 3 work)

Phase 3 depends on correct tsvector parsing. Three pre-existing bugs will cause silent data corruption if not fixed first.

### 0.1: Fix `parse_tsvector_entry()` — multi-position corruption

**File**: `src/sql/fts.rs:579-599`

**Bug**: The current parser silently corrupts multi-position entries:

```rust
// CURRENT (broken)
let weight = pos_weight.chars().last().unwrap_or('A');
let pos_str: String = pos_weight.chars().filter(|c| c.is_ascii_digit()).collect();
let pos: u32 = pos_str.parse().unwrap_or(1);
```

| Input | Expected | Actual (bug) |
|---|---|---|
| `'word':1,3,5A` | positions [1,3,5] weight A | pos_str="135", pos=135 |
| `'word':1,3` (English) | positions [1,3] weight D | weight='3' (digit!), pos_str="13", pos=13 |

**Fix**: Rewrite as `parse_tsvector_entries()` returning `Vec<(String, Vec<(u32, char)>)>`:
- Split position part on `,`
- For each position segment: extract digits as position number, optional trailing `[A-D]` as weight (default 'D')
- Return all positions per word

### 0.2: Fix `setweight()` — multi-position corruption

**File**: `src/sql/expr/functions/fts.rs:50-63`

**Bug**: Same root cause as 0.1:
```rust
let pos_num: String = pos_part.chars().filter(|c| c.is_ascii_digit()).collect();
format!("{}:{}{}", word_part, pos_num, weight)
```

For `'word':1,3,5`: `pos_num` = `"135"` → produces `'word':135A` → **data corruption**.

**Fix**: Parse all positions, apply weight to each, re-emit as `'word':1X,3X,5X` where X is the new weight.

### 0.3: Fix non-English `to_tsvector` weight output

**File**: `src/sql/fts.rs` — tsvector formatting functions

**Bug**: Non-English configs (CJK) hardcode weight 'A' in output: `'数据库':1A`. PostgreSQL's `to_tsvector` always produces default weight D (displayed as no suffix). Weight A should only appear after explicit `setweight()`.

**Fix**: Remove hardcoded 'A' suffix from non-English tsvector formatting. Output `'数据库':1` (matching PG behavior).

**Impact**: Existing stored tsvectors with 'A' suffix are not corrupted in meaning (they still work for presence-based matching). The new position-aware parser (prerequisite section) handles both formats. Users who need correct weight-based ranking should REINDEX after this fix. Document this in release notes.

### 0.4: Verification

After Task 0 fixes:
- All 5 existing FTS regression tests (130, 131, 218, 219, 236) must pass
- Test 237 (tokenizers) must pass
- `cargo test` must pass
- Spot-check: `SELECT to_tsvector('simple', 'hello world')` should produce `'hello':1 'world':2` (no weight suffix)

---

## Pre-requisite: Tsvector Position Parser (MUST do after Task 0)

### Problem Statement

Phase 3 phrase matching requires position information. The current codebase discards all positions:
- `extract_tsvector_words()` → returns `HashSet<String>` (discards ALL positions)
- `parse_tsvector_entry()` → broken for multi-position entries (fixed in Task 0)

### Solution: Unified Position-Aware Tsvector Parser

**New function**: `extract_tsvector_words_with_positions(tsvector: &str) -> HashMap<String, Vec<(u32, char)>>`

Returns word → sorted list of (position, weight) tuples.

Must handle ALL formats (post-Task 0 fix, both old and new data):
- `'word':1` → position 1, weight D (default — no suffix means D, NOT A)
- `'word':1A` → position 1, weight A (explicit weight)
- `'word':1,3,5` → positions [1,3,5], all weight D
- `'word':1A,3B,5` → position 1 weight A, position 3 weight B, position 5 weight D
- `'word':1,3,5A` → positions [1,3,5], weight A on last only (PG format for setweight)

**Weight default is D, not A** — matching PostgreSQL. In PG, omitted weight = D.

**Drop the thin wrapper** — always use `HashMap<String, Vec<u32>>` via `extract_positions_only()`. The performance difference between HashMap and HashSet is negligible for typical tsvector sizes, and it eliminates the need to branch on whether the query contains phrase operators. Only `extract_tsvector_words_with_positions()` (full version with weights) and `extract_positions_only()` (convenience wrapper) are needed.
**Convenience wrapper for phrase matching** (positions only, no weights):
```rust
fn extract_positions_only(tsvector: &str) -> HashMap<String, Vec<u32>> {
    extract_tsvector_words_with_positions(tsvector)
        .into_iter()
        .map(|(word, pws)| (word, pws.into_iter().map(|(p, _)| p).collect()))
        .collect()
}
```

### Callers to update
| Caller | Current | New |
|---|---|---|
| `ts_match()` / `match_tsquery()` | `extract_tsvector_words()` → `HashSet<String>` | Always use `extract_positions_only()` → `HashMap<String, Vec<u32>>` |
| `compute_rank()` | `extract_tsvector_words()` | Use `extract_tsvector_words_with_positions()` (needs weights for ranking) |
| `concat_tsvector()` | `parse_tsvector_entry()` | Use fixed `parse_tsvector_entries()` from Task 0 |
| `setweight()` | Direct string manipulation | Use fixed parser from Task 0 |
| `validate_tsquery_syntax()` | Creates empty `HashSet` | Change to empty `HashMap` |
### Files Changed
- `src/sql/fts.rs`: Add `extract_tsvector_words_with_positions()`, `extract_positions_only()`. Remove `extract_tsvector_words()` (inline callers to use `extract_positions_only()`).

---

## Task 3.1: `phraseto_tsquery()` + `<->` / `<N>` Phrase Operators

### 3.1a: Extend `TsQueryToken` Enum

**File**: `src/sql/fts.rs`

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
enum TsQueryToken {
    Not,
    And,
    Or,
    FollowedBy(u32),  // NEW — <-> = FollowedBy(1), <N> = FollowedBy(N)
    LParen,
    RParen,
    Term(String),
}
```

### 3.1b: Fix tokenizer break characters + parse `<->` / `<N>`

**CRITICAL (from Metis C4)**: The character `<` is NOT in the current operator break set:

```rust
// CURRENT (broken for <->)
if c.is_whitespace() || matches!(c, '!' | '&' | '|' | '(' | ')') {
    break;
}
```

Input `'hello' <-> 'world'` currently produces `Term("<->")` — parsed as a literal term, not an operator. This is the **highest-priority blocker** for phrase support.

**Fix**: Add `<` to the break set in BOTH tokenizers:

```rust
if c.is_whitespace() || matches!(c, '!' | '&' | '|' | '(' | ')' | '<') {
    break;
}
```

**CRITICAL (from Metis C3)**: There are TWO duplicate tsquery tokenizers that MUST be updated in sync:
1. `src/sql/fts.rs:327-398` — `tokenize_tsquery()` (runtime evaluation)
2. `src/sql/planner/gin_predicate.rs:301-372` — `tokenize_tsquery_for_planner()` (GIN qual extraction)

**Strategy**: Extract a shared `tokenize_tsquery_common()` into `fts.rs` that both callers use. The planner version wraps this to extract GIN-relevant terms. This eliminates divergence risk.

**Parsing `<->` and `<N>`** — when encountering `<`:
1. Peek: if next chars are `->` → consume → `FollowedBy(1)`
2. Else: try to parse `<digits>` followed by `>` → `FollowedBy(digits)`
3. `<0>` is invalid (PG rejects it) → return error
4. If neither pattern matches, treat `<` as regular text (part of a term)

### 3.1c: Refactor `TsQueryEvaluator` for Position-Aware Matching

**File**: `src/sql/fts.rs`

Current evaluator signature:
```rust
struct TsQueryEvaluator<'a> {
    tokens: &'a [TsQueryToken],
    pos: usize,
    words: &'a HashSet<String>,  // presence only
}
```

New evaluator:
```rust
struct TsQueryEvaluator<'a> {
    tokens: &'a [TsQueryToken],
    pos: usize,
    word_positions: &'a HashMap<String, Vec<u32>>,  // word → sorted positions
}
```

**Evaluation result type** (CORRECTED from Round 2 — must carry negation for `!term <-> term`):

The original `MatchResult { Bool(bool), Positions(Vec<u32>) }` design has a **BLOCKING flaw**: when `parse_unary` sees `!`, it would collapse `Positions([1,3])` to `Bool(false)`, **losing the position information** that the phrase evaluator needs for `!'cat' <-> 'dog'`.

PG handles this by checking: "for each position of 'dog', is 'cat' NOT at the preceding position?" This requires knowing cat's positions even when negated.

**Fixed design** — unified `EvalResult` struct:
```rust
struct EvalResult {
    positions: Vec<u32>,  // positions where the term appears (or all positions for special cases)
    negated: bool,        // whether this is under a NOT operator
}

impl EvalResult {
    fn is_match(&self) -> bool {
        if self.negated {
            self.positions.is_empty()  // NOT: true if term is absent from document
        } else {
            !self.positions.is_empty() // true if term is present
        }
    }

    fn term(positions: Vec<u32>) -> Self {
        Self { positions, negated: false }
    }

    fn negate(mut self) -> Self {
        self.negated = !self.negated;
        self
    }
}
```

**How this handles all cases:**

| Expression | Left | Right | Phrase logic |
|---|---|---|---|
| `a <-> b` | `{pos:[1], neg:false}` | `{pos:[2], neg:false}` | Normal: find pb where some pa satisfies pb-pa==N |
| `!a <-> b` | `{pos:[1], neg:true}` | `{pos:[2], neg:false}` | Negated left: find pb where NO pa satisfies pb-pa==N |
| `a <-> !b` | `{pos:[1], neg:false}` | `{pos:[2,4], neg:true}` | Negated right: find pa where NO pb satisfies pb-pa==N |
| `(a <-> b) <-> c` | `{pos:[2], neg:false}` (from inner phrase) | `{pos:[3], neg:false}` | Normal nested: positions propagate |

**Key behaviors:**
- Term absent + negated: `{pos:[], neg:true}` → `is_match()` = true (NOT of absent = true)
- In phrase context with negated empty positions: "no pa satisfies condition" is trivially true for all pb → all pb positions pass
- AND/OR use `is_match()` for boolean reduction; phrase uses raw `positions` + `negated` flag

**`parse_unary` returns `EvalResult`**: `!expr` → `expr.evaluate().negate()` — preserves positions, flips flag.

**`parse_and` returns `EvalResult`**: `left & right` → `EvalResult::term(vec![])` with `negated = !(left.is_match() && right.is_match())`. Wait — simpler: AND/OR always produce non-negated results:
```rust
// parse_and
let matched = left.is_match() && right.is_match();
EvalResult { positions: if matched { /* merge */ } else { vec![] }, negated: false }
```
Actually, AND/OR at the top level just need boolean semantics. Only `parse_phrase` needs the full `EvalResult` with positions and negation. So:
- `parse_primary` → `EvalResult::term(word_positions[term].clone())`
- `parse_unary` → `result.negate()` for NOT
- `parse_phrase` → position-aware matching with negation handling
- `parse_and` → `EvalResult { positions: vec![], negated: !(left.is_match() && right.is_match()) }` — but this means `a & b` returns empty positions. If `(a & b) <-> c` is ever needed... but with precedence `<->` > `&`, this can't happen syntactically. `a & b <-> c` = `a & (b <-> c)`. So AND never feeds into phrase. ✓
- `parse_or` → same boolean reduction. OR also never feeds into phrase. ✓

**All 5 callers** of the evaluator (from Metis C5):
1. `ts_match()` (line 266) — used for `@@` evaluation
2. `match_tsquery()` (line 299) — passes words to evaluator
3. `TsQueryEvaluator::new()` (line 420) — takes `&HashSet<String>`
4. `validate_tsquery_syntax()` (line 291) — creates empty HashSet for syntax check → change to empty HashMap
5. `compute_rank()` (line 602) — uses `extract_tsvector_words()` directly → will use position-aware version

### 3.1d: Implement `phraseto_tsquery()` Function

**File**: `src/sql/fts.rs`

```rust
pub fn phraseto_tsquery(args: Vec<Value>) -> Result<Value>
```

**Behavior** (matching PG 17, from `tsquery_cleanup.c` L199-357):
1. Tokenize input text using the specified config (with stemming)
2. Track positions of ALL tokens (including stopwords)
3. Remove stopwords, compute **exact distances** between surviving terms
4. **Distance = pos(next_surviving_term) - pos(previous_surviving_term)**

**Exact PG examples**:
```sql
phraseto_tsquery('english', 'The Fat Rats')      → 'fat' <-> 'rat'
-- 'the' is stopword at pos 1, 'fat' at pos 2, 'rats' stemmed to 'rat' at pos 3
-- distance(fat, rat) = 3-2 = 1, so <->

phraseto_tsquery('english', 'The Cat and Rats')   → 'cat' <2> 'rat'
-- 'the' at 1 (stop), 'cat' at 2, 'and' at 3 (stop), 'rats'→'rat' at 4
-- distance(cat, rat) = 4-2 = 2, so <2>

phraseto_tsquery('english', 'cat')                → 'cat'
-- Single word, no operator

phraseto_tsquery('english', 'the')                → (empty tsquery)
-- All stopwords → empty

phraseto_tsquery('simple', 'hello world')         → 'hello' <-> 'world'
-- 'simple' config has no stopwords
```

5. Single-word input → just `'word'` (no operator)
6. Empty input / all stopwords → empty tsquery (return empty string, not error)
7. **`<N>` means EXACTLY N positions apart** (not "within N")

**Stopword detection**: Use `is_stopword_for_config(config, word)`:
- `simple` config → no stopwords
- `english` config → 4-word minimal list (`is_simple_stopword`)
- `english_stem` config → full PG 174-word list (`is_english_stopword`)
- CJK configs → no stopwords (CJK tokenizers don't use stopword lists)

**Register**: `src/sql/expr/functions/fts.rs` — `map.insert("PHRASETO_TSQUERY", phraseto_tsquery);`
**Type signature**: `src/sql/types/registry/misc.rs` — `FunctionSignature::fixed(DataType::Tsquery).with_args(1, Some(2))`

### 3.1e: Update GIN Predicate Analysis

**File**: `src/sql/planner/gin_predicate.rs`

After tokenizer unification (3.1b), the shared tokenizer handles `<->` / `<N>`. The GIN qual extraction logic needs:
- Treat `FollowedBy(N)` as equivalent to `And` for GIN candidate filtering (AND semantics — expand candidates, never shrink)
- Set `recheck_needed = true` when ANY phrase operator is present (position recheck against base row)
- **MUST NOT** try to evaluate phrase semantics at the GIN level — GIN only stores token hashes, not positions

**From Metis F2**: Without this fix, a phrase query through GIN would either:
- Parse `<->` as a term → look for literal `<->` in index → zero results → **false negative**
- Or fail to parse → no GIN scan → seq scan (defeats the purpose)

### 3.1f: Update Trigger Validation

**File**: `src/sql/triggers/cache.rs`

Remove `"phraseto_tsquery"` from `UNSUPPORTED_FTS_FUNCTIONS` list.

---

## Task 3.2: `websearch_to_tsquery()`

### Parsing Rules (PG 17 compatible)

| Input syntax | tsquery output | Rule |
|---|---|---|
| `word1 word2` | `'word1' & 'word2'` | Unquoted words → AND |
| `"word1 word2"` | `'word1' <-> 'word2'` | Quoted phrase → `<->` chain |
| `-word` | `!'word'` | Prefix minus → NOT |
| `word1 or word2` | `'word1' \| 'word2'` | `or` / `OR` keyword → OR |
| `word1 OR word2` | `'word1' \| 'word2'` | Case-insensitive OR |

**Critical PG behavior**: `websearch_to_tsquery` **NEVER raises syntax errors**. Any garbage input returns a best-effort parse. This is by design — it's meant for end-user search boxes.

**Edge cases**:
- Empty string → empty tsquery
- Only stopwords → empty tsquery
- `""` (empty quotes) → ignored
- `-` alone → ignored
- Multiple negations: `-word1 -word2` → `!'word1' & !'word2'`
- Stemming applies to all terms (use config's tokenizer)
- Stopwords in phrases → `<N>` distance (same as `phraseto_tsquery`)
- Trailing `or` → ignored
- Leading `or` → ignored
- Unmatched `"` → treat rest of input as phrase
- Random punctuation → ignored / best-effort

**Stopword handling in phrases**: For `websearch_to_tsquery('english', '"the quick brown fox"')`:
- "the" is stopword → removed, gap computed
- Result: `'quick' <-> 'brown' <-> 'fox'` (distance adjusted for removed stopword)
- Uses same distance logic as `phraseto_tsquery`

**Implementation**: `src/sql/fts.rs`

```rust
pub fn websearch_to_tsquery(args: Vec<Value>) -> Result<Value>
```

**Register**: `src/sql/expr/functions/fts.rs` — `map.insert("WEBSEARCH_TO_TSQUERY", websearch_to_tsquery);`
**Type signature**: `FunctionSignature::fixed(DataType::Tsquery).with_args(1, Some(2))`

**Update trigger validation**: Remove `"websearch_to_tsquery"` from `UNSUPPORTED_FTS_FUNCTIONS`.

---

## Task 3.3: `ts_headline()` — Search Result Highlighting

### Function Signature

```sql
ts_headline([ config regconfig, ] document text, query tsquery [, options text ]) → text
```

**Implementation**: `src/sql/fts.rs`

```rust
pub fn ts_headline(args: Vec<Value>) -> Result<Value>
```

Args: 2-4 arguments:
- 2 args: `(document, query)` — use default config
- 3 args: `(config, document, query)` or `(document, query, options)` — disambiguate by type
- 4 args: `(config, document, query, options)`

### Options Parsing (PG-compatible defaults)

| Option | Default | Description |
|---|---|---|
| `StartSel` | `<b>` | Tag before highlighted word |
| `StopSel` | `</b>` | Tag after highlighted word |
| `MaxWords` | `35` | Max words in each fragment |
| `MinWords` | `15` | Min words in each fragment |
| `ShortWord` | `3` | Words shorter than this are dropped at fragment start/end |
| `HighlightAll` | `false` | If true, highlight entire document |
| `MaxFragments` | `0` | Max number of fragments (0 = whole document with highlights) |
| `FragmentDelimiter` | ` ... ` | Delimiter between fragments |

Options string format: `'StartSel=<em>, StopSel=</em>, MaxFragments=3'`

### Algorithm

1. Parse options string (comma-separated key=value pairs)
2. **Re-tokenize** the document using the config's tokenizer (NOT the query's config — from Metis F3)
3. Build a position→word mapping from the tokenized text
4. Identify which positions match the tsquery terms (case-insensitive, stemmed match)
5. If `MaxFragments=0` (default): highlight the entire document
   - Walk through the original text, wrap matching words with StartSel/StopSel
6. If `MaxFragments>0`: select best fragments
   - Score each possible fragment window by number of matching terms
   - Select top N non-overlapping fragments
   - Return fragments joined by FragmentDelimiter
7. If `HighlightAll=true`: highlight every occurrence in the entire document

**Key challenge**: Mapping tokenizer positions back to character offsets in original text.
**Approach**: Modify tokenizer interface to return character offsets alongside tokens. Add a `tokenize_with_offsets(text, config) -> Vec<(String, usize, usize)>` function that returns `(token_text, start_char_offset, end_char_offset)` for each token.

**CJK-specific handling**: CJK text has no word delimiters. Jieba splits "数据库系统" into ["数据库", "系统"]. Since jieba/ngram tokenizers produce exact substrings of the original text (no stemming), we can find each token's position via substring search in the original text. For English with stemming ("running" → "run"), we record the ORIGINAL word's offsets during tokenization, then match the stemmed form against query terms but highlight the original word.

**Implementation steps for `tokenize_with_offsets`**:
1. For `simple` config: split on whitespace, track char offsets as we go
2. For English configs: split on whitespace (track offsets), then stem — return `(stemmed_form, original_start, original_end)`
3. For jieba/ngram: the tokenizer produces substrings — scan the original text to find each substring's start offset (use `str::find` with progressive start position to handle duplicates)
4. For `chinese_ngram` (mixed): tokenize with jieba first (with offsets), then overlay bigrams (compute offsets from the jieba token offsets)

**Note (from Metis F3)**: If the user passes a different config to `ts_headline` than was used for the query, highlights may not align with matches. This matches PG behavior — it's the user's responsibility to use consistent configs. We document this, not fix it.

**Register**: `src/sql/expr/functions/fts.rs` — `map.insert("TS_HEADLINE", ts_headline);`
**Type signature**: `FunctionSignature::fixed(DataType::Text).with_args(2, Some(4))`

---

## Task 3.4: Improved `ts_rank` / `ts_rank_cd`

### Current State

- `ts_rank` and `ts_rank_cd` are BOTH mapped to the same basic `compute_rank()` function
- Accepts 2 args only (tsvector, tsquery)
- Returns simple overlap ratio: `matched_terms / total_terms * 0.0607927`
- Ignores weights (A/B/C/D)
- Ignores normalization parameter
- **Bug (from Metis F4)**: Uses naive string split `tsquery.split(|c| ['&', '|', '!'].contains(&c))` which breaks on `<->` syntax — `<` gets attached to terms

### PG 17 Signatures

```sql
ts_rank([ weights float4[], ] tsvector, tsquery [, normalization integer ]) → float4
ts_rank_cd([ weights float4[], ] tsvector, tsquery [, normalization integer ]) → float4
```

### Weight System (PG defaults)

| Weight | Default value |
|---|---|
| D | 0.1 |
| C | 0.2 |
| B | 0.4 |
| A | 1.0 |

Weights array is `{D, C, B, A}` — note the reversed order! Index 0=D, 1=C, 2=B, 3=A.

### Normalization Flags (bitmask)

| Flag | Effect | Applies to |
|---|---|---|
| 0 | Ignore document length | both |
| 1 | Divide rank by `1 + log(document_length)` | both |
| 2 | Divide rank by `document_length` | both |
| 4 | Divide rank by mean harmonic distance between extents | **ts_rank_cd only** |
| 8 | Divide rank by number of unique words | both |
| 16 | Divide rank by `1 + log(number of unique words)` | both |
| 32 | Divide rank by itself + 1 (`rank / (rank + 1)`) → maps to [0, 1) | both |

### `ts_rank` Algorithm

1. Parse tsvector with `extract_tsvector_words_with_positions()` (need weights)
2. Extract query terms using the **shared tokenizer** (NOT naive string split — fixes Metis F4)
3. For each matching query term:
   - Look up its positions and weights in the tsvector
   - Sum `weight_value(w) * term_frequency` across all occurrences
4. Apply normalization divisor based on flags

### `ts_rank_cd` Algorithm (Cover Density Ranking)

1. Parse tsvector with positions
2. Extract query terms
3. Find all "covers" — minimal spans containing all query terms
4. For each cover: score = sum of weights / (cover_length)²
5. Sum scores across all covers
6. Apply normalization
7. **If tsvector has no positions** (stripped): return 0.0 (PG behavior)

### Implementation Plan

**File**: `src/sql/fts.rs`

1. Rewrite `compute_rank()` to use position-aware parser and proper term extraction
2. Add `compute_rank_cd()` for cover density ranking
3. Register as separate functions: `ts_rank` → `compute_rank`, `ts_rank_cd` → `compute_rank_cd`

**Argument handling** (disambiguate 2-4 args):
- If first arg is an array (or text parseable as `{f,f,f,f}`) → treat as weights
- If last arg is integer → treat as normalization
- The tsvector and tsquery are always the two middle args

---

## Integration Points (Summary)

| File | Change | Task |
|---|---|---|
| `src/sql/fts.rs` | Fix parse_tsvector_entry, setweight; position parser; evaluator refactor; 3 new functions; improved rank | 0, prereq, 3.1-3.4 |
| `src/sql/expr/functions/fts.rs` | Fix setweight multi-position; register 3 new functions | 0.2, 3.1-3.3 |
| `src/sql/types/registry/misc.rs` | 3 new type signatures | 3.1-3.3 |
| `src/sql/planner/gin_predicate.rs` | Unify tokenizer; handle `<->` in GIN qual | 3.1b, 3.1e |
| `src/sql/triggers/cache.rs` | Remove 2 functions from UNSUPPORTED list | 3.1f, 3.2 |
| `tests/238_fts_phase3.sql` | Integration test | all |
| `tests/238_fts_phase3.assert` | Assertions | all |
| `scripts/regression_gate.list` | Add test 238 | all |

---

## Risk Analysis

### Critical Risks

1. **TsQueryEvaluator refactor breaking existing tests**: The evaluator changes from `HashSet<String>` to `HashMap<String, Vec<u32>>`. All existing AND/OR/NOT logic must keep working. Mitigated by: `words.contains(term)` → `word_positions.contains_key(term)` is semantically identical for non-phrase queries. Run ALL regression tests after every evaluator change.

2. **Tsvector position format inconsistency**: English produces `'word':1,3` (no weight), CJK produces `'word':1A` (with weight). The new position parser handles both via: each position segment = digits + optional `[A-D]`, default weight = D.

3. **Dual tokenizer divergence** (Metis C3): `tokenize_tsquery()` and `tokenize_tsquery_for_planner()` are near-identical copies. If we update one and forget the other, GIN scans silently fail. Mitigated by: extracting shared tokenizer (Task 3.1b).

4. **`<->` collision with pgvector**: The parser preprocessor rewrites `<->` to `l2_distance()`. This does NOT affect us because tsquery's `<->` is inside string literals (`to_tsquery('cat <-> dog')`), not in SQL expressions. Add a test to verify no collision.

5. **Nested phrase position propagation** (Metis F1): `(a <-> b) <-> c` requires propagating matched positions through the expression tree. The `EvalResult` struct carries positions forward. `!term <-> term` handled via `negated` flag (see 3.1c design).

6. **compute_rank naive string split** (Metis F4): Current `compute_rank()` splits on `['&', '|', '!']` which corrupts terms containing `<->`. After Task 3.4, this is replaced with proper tokenizer-based extraction.

### Design Decisions (Pre-resolved)

1. **Phrase matching is in-memory only** — GIN indexes don't store positions. For GIN-accelerated phrase queries, the GIN scan uses AND semantics (word presence) with `recheck_needed=true`. The actual position check happens during recheck against the base row's tsvector.

2. **`<->` precedence > `&` precedence** — confirmed from PG source `tsquery.c` L29-35. Priority: `!`(4) > `<->`(3) > `&`(2) > `|`(1). They are parsed at SEPARATE levels in the recursive descent parser (`parse_phrase` between `parse_and` and `parse_unary`).

3. **EvalResult with negation** — Unified `EvalResult { positions, negated }` replaces the original `MatchResult { Bool, Positions }` design. The `negated` flag enables correct `!term <-> term` handling without losing position information. AND/OR reduce to boolean via `is_match()`; phrase uses raw positions + negation flag.

4. **ts_headline Approach B** (tokenizer-based offset tracking) — for both `MaxFragments=0` and `MaxFragments>0`. Simple string matching (Approach A) has edge cases with stemmed words and overlapping tokens.

5. **Weight format fix** — New tsvectors omit default weight suffix (matching PG). Parser tolerates both old format (`'word':1A`) and new (`'word':1`). Existing data works; users should REINDEX for correct weight-based ranking.

6. **websearch_to_tsquery never errors** — By PG design, any garbage input returns best-effort parse. Never raise syntax errors from this function.

7. **Always HashMap, never HashSet** — `extract_tsvector_words()` (HashSet) is removed. All callers use `extract_positions_only()` (HashMap) or `extract_tsvector_words_with_positions()` (HashMap with weights). This eliminates branching on whether the query contains phrase operators.

8. **Known unsupported features (out of scope for Phase 3)**:
   - `:*` prefix matching in tsquery (`'cat':*`) — GIN with hash keys cannot do prefix scan
   - Weight-qualified terms in tsquery (`'cat':A`) — not in current TsQueryToken enum
   - These are pre-existing limitations. Document in release notes if asked about.

---

## Execution Order

1. **Task 0**: Fix pre-existing bugs (parse_tsvector_entry, setweight, non-English weight output)
2. **Prerequisite**: Unified position parser (`extract_tsvector_words_with_positions`)
3. **Task 3.1a-b**: TsQueryToken extension + tokenizer unification + `<` break char fix
4. **Task 3.1c**: Evaluator refactor (EvalResult with negation, parse_phrase, position-aware matching)
5. **Task 3.1d**: `phraseto_tsquery()` function
6. **Task 3.1e**: GIN predicate update for `<->`
7. **Task 3.1f**: Remove from UNSUPPORTED list
8. **Task 3.2**: `websearch_to_tsquery()` function (depends on 3.1 for `<->` support)
9. **Task 3.3**: `ts_headline()` function
10. **Task 3.4**: `ts_rank`/`ts_rank_cd` improvements (depends on position parser)
11. **Integration**: Wire up registrations, test 238, regression check all FTS tests

---

## Acceptance Test Plan (SQL-based, no manual verification)

All tests run against db9-server. Expected outputs verified against PostgreSQL 17.7 first.

### Task 0 verification
```sql
-- Multi-position tsvector preserved correctly
SELECT setweight(to_tsvector('simple', 'the fat cat sat on the mat'), 'A');
-- Expected: 'cat':4A 'fat':3A 'mat':7A 'on':5A 'sat':4A 'the':1A,6A  (or similar with correct multi-positions)
```

### Task 3.1 verification
```sql
-- Phrase match (adjacent)
SELECT to_tsvector('simple', 'distributed database systems') @@ phraseto_tsquery('simple', 'distributed database');
-- Expected: t

-- Phrase non-match (wrong order)
SELECT to_tsvector('simple', 'database distributed systems') @@ phraseto_tsquery('simple', 'distributed database');
-- Expected: f

-- Distance operator
SELECT to_tsvector('simple', 'distributed key value database') @@ to_tsquery('simple', '''distributed'' <3> ''database''');
-- Expected: t

-- Nested phrase
SELECT to_tsvector('simple', 'the quick brown fox') @@ to_tsquery('simple', '''quick'' <-> ''brown'' <-> ''fox''');
-- Expected: t

-- Stopword distance in phraseto_tsquery
SELECT phraseto_tsquery('english', 'The Cat and Rats');
-- Expected: 'cat' <2> 'rat'

-- GIN index with phrase (verify via EXPLAIN)
-- CREATE GIN index, run phrase query, verify GinScan is used
```

### Task 3.2 verification
```sql
SELECT websearch_to_tsquery('english', 'fat cat');
-- Expected: 'fat' & 'cat'

SELECT websearch_to_tsquery('english', '"fat cat"');
-- Expected: 'fat' <-> 'cat'

SELECT websearch_to_tsquery('english', 'fat -cat');
-- Expected: 'fat' & !'cat'

SELECT websearch_to_tsquery('english', 'fat or cat');
-- Expected: 'fat' | 'cat'

-- Never errors on garbage
SELECT websearch_to_tsquery('english', '!!!***');
-- Expected: (empty tsquery, no error)
```

### Task 3.3 verification
```sql
SELECT ts_headline('english', 'The fat cat sat on the mat', to_tsquery('english', 'cat'));
-- Expected: The fat <b>cat</b> sat on the mat

SELECT ts_headline('english', 'The fat cat sat on the mat', to_tsquery('english', 'cat'), 'StartSel=<em>, StopSel=</em>');
-- Expected: The fat <em>cat</em> sat on the mat
```

### Task 3.4 verification
```sql
-- Basic ts_rank with weights
SELECT ts_rank(to_tsvector('simple', 'hello world'), to_tsquery('simple', 'hello'));
-- Expected: non-zero float

-- ts_rank_cd returns different value than ts_rank
SELECT ts_rank_cd(to_tsvector('simple', 'hello world'), to_tsquery('simple', 'hello'));
-- Expected: non-zero float (different from ts_rank)

-- Normalization flag 32
SELECT ts_rank(to_tsvector('simple', 'hello world'), to_tsquery('simple', 'hello'), 32);
-- Expected: value in [0, 1) range
```
