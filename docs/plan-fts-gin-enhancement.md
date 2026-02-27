# FTS / GIN 索引增强开发计划

> **目标**：实现用户无感的全文检索（CJK + 英文混杂），n-gram 支持，模糊匹配能力，zhparser 兼容。
>
> **原则**：PostgreSQL is the specification。所有行为以 PG 17 为标准。

---

## 现状摘要

| 能力 | 状态 |
|------|------|
| GIN 索引写入（INSERT/UPDATE/DELETE 维护 token） | ✅ 完整 |
| GIN 索引查询（Index Scan 加速） | ❌ **完全禁用** — 所有 `@@` / `@>` 全表扫描 |
| tokenizer: simple/english | ✅ 极简（4 个 stopword，无 stemming） |
| tokenizer: chinese/jieba | ✅ 基本可用（词级分词，不支持子串匹配） |
| tokenizer: zhparser 兼容 | ❌ 缺失 |
| tokenizer: n-gram | ❌ 缺失 |
| `phraseto_tsquery` / `<->` 短语 | ❌ 缺失 |
| `websearch_to_tsquery` | ❌ 缺失 |
| `ts_headline` 高亮 | ❌ 缺失 |
| `LIKE '%x%'` 索引加速 (pg_trgm) | ❌ 缺失 |
| 模糊匹配 (similarity/levenshtein) | ❌ 缺失 |

---

## Review Notes（先校对这些再开工）

> 目的：避免实现后出现 **false negative（漏结果）**、语义偏离 PostgreSQL、或引入“运行时偷偷换计划”的不可诊断行为。

1. **GIN Scan 正确性底线：候选集只能放大，不能缩小**  
   - 当前 Phase 1 的“多 token 取交集（AND 语义）”只对纯 AND 查询成立；对 `OR`、`NOT`、数组 `&&`（overlap = OR 语义）会直接漏结果，recheck 也救不回来。  
   - 建议：要么在 Phase 1 明确只支持 **AND-only 的 indexable 子集**（例如 `plainto_tsquery()` 产生的 `&` 链 + `@>` containment），要么把 token 组合关系下沉为一个布尔表达式（AND/OR/NOT）并按布尔语义做 posting list 的集合运算。

2. **不要做运行时“fallback 到全表扫描”**  
   - 运行时从 `GIN Index Scan` 变成 `Seq Scan` 会导致 `EXPLAIN` 与真实执行不一致，诊断成本很高，也违背“无隐藏 fallback”。  
   - 内存/候选集爆炸要靠：流式集合运算（不物化所有 PK）、分批 `batch_get`、以及 planner 的代价模型来做决策；必要时宁可报错（可控、可诊断）也不要静默换计划。

3. **tsquery / opclass 语义要对齐 PG 的“支持范围”**  
   - `@@`：建议先覆盖 `&`/`|` 组合与 `A & !B`（差集）这类可 index 的布尔式；`!A`（纯否定）一般不可用索引，需要 planner 禁用。  
   - `<->` / `<N>`（phrase/proximity）即便走索引也应当 `Recheck Cond`（GIN 不存位置）。  
   - `:*` 前缀匹配：如果 GIN key 使用 hash（而非 lexeme 字符串），天然无法做 prefix 扫描；要么明确不支持，要么调整索引键设计（会影响存量索引格式，需单独立项）。

4. **“兼容 DDL”不要 silent accept**  
   - `CREATE TEXT SEARCH CONFIGURATION ...` 如果直接 no-op，会把错误推迟到运行期，排查很难。  
   - 建议：只对“我们确定能等价映射”的 zhparser 场景做兼容（并持久化 config→tokenizer 映射，按租户隔离）；其余场景返回 PG 风格的 `0A000 feature_not_supported`。

---

## Phase 1 — GIN Index Scan（让索引真正工作）

**目标**：`@@` / `@>` / `&&` 查询能利用 GIN 索引，而非全表扫描。
**预估工作量**：5-7 天

### Task 1.1: ScanType 增加 GinScan 变体

**文件**: `src/sql/planner/mod.rs`

```rust
pub enum GinQual {
    Term { token_hash: u64 },
    And(Vec<GinQual>),
    Or(Vec<GinQual>),
    Not(Box<GinQual>),
}

pub enum ScanType {
    // ... existing variants ...
    GinIndexScan {
        index_id: u64,
        index_name: String,
        /// Boolean expression of token hashes (MUST be a superset filter; never allow false negatives)
        qual: GinQual,
        /// Whether a per-row recheck is required (phrase/proximity, lossy tokenization, hash collisions, etc.)
        recheck_needed: bool,
    },
}
```

**验收标准**：
- `ScanType::GinIndexScan` 编译通过
- EXPLAIN 输出中显示 `GIN Index Scan using <index_name>` 和 `Recheck Cond`

### Task 1.2: Predicate 分析 — 识别 GIN 可加速的谓词

**文件**: `src/sql/planner/predicate.rs` (新增 GIN 谓词分析)

从 `TypedExpr` 中识别以下模式：
- `col @@ tsquery_expr` → 提取 tsquery tokens
- `col @> jsonb_literal` → 提取 JSONB GIN tokens
- `col @> array_literal` → 提取 ARRAY GIN tokens
- `col && array_literal` → 提取 ARRAY overlap tokens（OR 语义）
- `to_tsvector('config', col) @@ tsquery_expr` → 匹配表达式索引

Token 提取逻辑：
- 对 `@@`：解析 tsquery → 生成 `GinQual`（AND/OR/NOT）+ lexeme hashes（`hash_tsvector_lexeme()`）
  - 约束：planner 只允许“可由索引产生候选集”的子集（例如存在至少一个正向 term；纯否定 `!A` 禁用）
- 对 JSONB `@>`：对常量侧调 `extract_gin_tokens()` 得到 token_hashes
- 对 ARRAY `@>`：对常量侧调 `extract_array_gin_tokens()` 得到 token_hashes
- 对 ARRAY `&&`：同上，但组合语义为 OR（并集）

**验收标准**：
- 单元测试：`WHERE body @@ plainto_tsquery('hello world')` → 解析为 `GinQual::And([Term(h1), Term(h2)])`
- 单元测试：`WHERE data @> '{"key": "val"}'` → 返回正确 token hashes
- 表达式索引匹配：`to_tsvector('chinese', col) @@ ...` 能正确匹配 `CREATE INDEX ... USING GIN (to_tsvector('chinese', col))`

### Task 1.3: Planner 集成 — GIN 索引选择

**文件**: `src/sql/planner/index_selection.rs`

修改 `choose_best_access_path_with_typed_filter()`：
1. 移除 `is_planner_usable_index()` 中对 GIN 的过滤（目前 line 99 只放行 btree）
2. 新增 `evaluate_gin_index()` 函数：
   - 检查索引是否 GIN（`method == "gin"`）
   - 检查过滤条件中是否有匹配的 GIN 谓词（`@@` / `@>` / `&&`）
   - 对表达式索引，匹配 `to_tsvector(config, col)` 形式
   - 估算代价：`GIN_BASE_COST + estimated_matches * ROW_FETCH_COST`

代价模型（`src/sql/planner/cost_model.rs`）新增：
```rust
pub const GIN_SCAN_BASE_COST: f64 = 4.0;
pub const GIN_TOKEN_SCAN_COST: f64 = 1.0;    // per token prefix scan
pub const GIN_ROW_FETCH_COST: f64 = 1.0;     // per row batch fetch
pub const GIN_SELECTIVITY: f64 = 0.01;        // default: 1% of rows
```

> NOTE: 上述常量建议仅作为“无统计信息时”的 fallback。已有 `TableStatsCache` + selectivity estimation（#790），优先用列统计估算 `estimated_matches`，确保 planner 决策更稳定。

**验收标准**：
- `EXPLAIN SELECT ... WHERE body @@ plainto_tsquery('hello')` → 显示 `GIN Index Scan`
- `EXPLAIN SELECT ... WHERE data @> '{"k":"v"}'` → 显示 `GIN Index Scan`
- GIN scan 代价低于 FullTableScan 代价时被选中
- 现有 B-tree 索引选择不受影响

### Task 1.4: GinScanOperator 实现

**新文件**: `src/sql/operators/gin_scan.rs`

实现 `PhysicalOperator` trait 的 `GinScanOperator`：

```
open():
  1. 对 GinQual 中涉及的每个 term 执行 posting list 扫描（按 token_hash 前缀 range scan）
     key_prefix = encode_gin_index_prefix_v2(db_id, table_id, index_id, token_hash)
     scan [key_prefix, key_prefix + 0xFF) → 得到有序 PK 流
  2. 按 GinQual 的布尔语义对 posting list 做集合运算：
     - AND → 交集
     - OR  → 并集
     - A & !B → 差集（需要正向锚点；不支持纯否定/需要全体集合的情况）
     要求：候选集必须是“可能命中行”的超集（只允许 false positive）
  3. 流式产生候选 PK（避免一次性物化所有 PK）
  4. 分批按 PK 取行（batch_get_rows），并执行 recheck + 额外 WHERE 过滤

next():
  返回下一行（已 recheck 过滤的）

close():
  释放资源
```

**关键实现细节**：
- 使用 `TikvStore::scan_prefix()` 做 GIN 前缀扫描
- PK 从 GIN key 的 `{SEP}{pk_values}` 后缀解码
- 集合运算尽量按“最选择性 term”优先（可先做轻量级计数/采样或利用 stats 估计）
- Recheck：对 `@@` 调 `ts_match()`，对 `@>` 调 `jsonb::contains()`
- 内存限制：不要运行时静默 fallback 到全表扫描；优先用流式集合运算 + 分批 batch_get 控制内存占用

**新增存储接口** (`src/storage/tikv_store/indexes.rs`):
```rust
/// Scan GIN index for all PKs matching a token hash.
pub async fn scan_gin_index_entries(
    &self,
    txn: &mut Transaction,
    db_id: u64,
    table_id: u64,
    index_id: u64,
    token_hash: u64,
) -> Result<Vec<Vec<u8>>>  // 初版可 Vec；若 posting list 大，尽快演进为分页/迭代式 API
```

**验收标准**：
- 10K 行表上 `@@` 查询使用 GIN scan，性能优于全表扫描
- `@>` JSONB/ARRAY 查询正确使用 GIN scan
- Recheck 确保无漏报（false negative = 0）
- 空结果集不报错
- NULL 值正确处理

### Task 1.5: Optimizer Build 层集成

**文件**: `src/sql/optimizer/build/scan.rs`

在 `build_scan_operator()` 中增加 `ScanType::GinIndexScan` 分支：
- 构造 `GinScanOperator`
- 传入 `GinQual` + recheck 谓词
- 如果有额外 WHERE 条件（非 GIN 谓词），在 GinScan 之上叠加 FilterOperator

**文件**: `src/sql/optimizer/physical_planner/mod.rs`

移除 line ~293 的 "GIN access-path planning is intentionally disabled" 注释及相关跳过逻辑。

**验收标准**：
- 端到端：`SELECT * FROM docs WHERE body @@ plainto_tsquery('hello')` 走 GIN scan 且结果正确
- 混合谓词：`WHERE body @@ query AND status = 'active'` 正确处理（GIN scan + filter）

### Task 1.6: 修复 `default_text_search_config` GUC Bug

**文件**: `src/sql/fts_tokenizers.rs` + `src/sql/fts.rs`

**Bug**: `default_text_search_config()` 使用 `OnceLock`，初始化后永远不变。`SET default_text_search_config = 'chinese'` 无效果。

**修复**:
- `to_tsvector()` / `plainto_tsquery()` / `to_tsquery()` 的单参数版本，应从当前 session 的 GUC 获取 config，而非 `OnceLock`
- `default_text_search_config()` 的 `OnceLock` 仅用作 fallback（当没有 session context 时）
- 传入 `QueryContext` 或通过 task-local 读取 session 的 `default_text_search_config` 设置

**验收标准**：
```sql
SET default_text_search_config = 'chinese';
SELECT to_tsvector('我爱中国');  -- 应使用 jieba 分词
RESET default_text_search_config;
SELECT to_tsvector('我爱中国');  -- 应使用 simple 分词（回退默认）
```

### Task 1.7: 更新文档和测试

- 更新 `docs/sot/extensions-gin.md`：将 `[Experimental] GIN planner/executor access path is currently disabled` 改为 `[Stable]`
- 新增集成测试 `tests/xxx_gin_index_scan.sql`：
  - JSONB `@>` with GIN
  - ARRAY `@>` with GIN
  - TSVECTOR `@@` with GIN (plain column + expression index)
  - EXPLAIN 验证 GIN Index Scan
  - 混合谓词测试
  - 大表性能对比
  - 空表 / NULL 值边界

---

## Phase 2 — Tokenizer 增强（让搜索结果符合预期）

**目标**：解决"搜'数据'匹配不到'数据库'"、"搜'running'匹配不到'run'"等核心 UX 问题。
**预估工作量**：4-5 天

### Task 2.1: N-gram Tokenizer 实现

**文件**: `src/sql/fts_tokenizers.rs`

新增 n-gram tokenizer：

```rust
fn tokenize_ngram(text: &str) -> Vec<String> {
    // 配置：n=2 (bigram) 为默认
    tokenize_ngram_with_size(text, 2)
}

fn tokenize_ngram_with_size(text: &str, n: usize) -> Vec<String> {
    let chars: Vec<char> = text.to_lowercase().chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if chars.len() < n {
        return vec![chars.iter().collect()];
    }
    chars.windows(n)
        .map(|w| w.iter().collect::<String>())
        .collect()
}
```

> NOTE: 上面代码片段只过滤了 whitespace；如果要满足“空白和标点作为分隔”的设计决策，需要先按“token 字符/分隔符”做分段，再对每段做 n-gram（避免跨标点生成 token）。

注册配置名：
- `"ngram"` → bigram (n=2)
- `"ngram3"` / `"trigram"` → trigram (n=3)

**设计决策**：
- CJK 字符按 Unicode char 边界切 n-gram
- 英文字母连续序列也参与 n-gram（与 pg_bigm 行为一致）
- 空白和标点作为分隔不参与 token
- 单字符（n=1, unigram）暴露为 `"unigram"` 配置，用于最大召回

**验收标准**：
```sql
SELECT to_tsvector('ngram', '数据库技术');
-- 结果包含: '数据' '据库' '库技' '技术'

SELECT to_tsvector('ngram', '数据库技术') @@ plainto_tsquery('ngram', '数据');
-- true（'数据' 是 '数据库技术' 的 bigram 之一）

SELECT to_tsvector('ngram', 'database') @@ plainto_tsquery('ngram', 'data');
-- true（query 侧也按 n-gram 生成 tsquery：'da' & 'at' & 'ta'）
```

### Task 2.2: zhparser 兼容（jieba tokenizer 别名）

**文件**: `src/sql/fts_tokenizers.rs`

将 `zhparser` 注册为 jieba tokenizer 的别名：

```rust
fn init_tokenizers() -> HashMap<String, TokenizerFn> {
    let mut map = HashMap::with_capacity(8);
    map.insert("simple".to_string(), tokenize_simple as TokenizerFn);
    map.insert("english".to_string(), tokenize_simple as TokenizerFn);
    map.insert("chinese".to_string(), tokenize_jieba as TokenizerFn);
    map.insert("jieba".to_string(), tokenize_jieba as TokenizerFn);
    map.insert("zhparser".to_string(), tokenize_jieba as TokenizerFn);    // NEW
    map.insert("ngram".to_string(), tokenize_ngram as TokenizerFn);       // NEW
    map.insert("ngram3".to_string(), tokenize_trigram as TokenizerFn);    // NEW
    map.insert("trigram".to_string(), tokenize_trigram as TokenizerFn);   // NEW
    map
}
```

同时需要处理 `CREATE EXTENSION zhparser` 的兼容：
- **文件**: `src/sql/executor/extensions.rs`
- `CREATE EXTENSION zhparser` 应成功但为 no-op（内置支持）
- `DROP EXTENSION zhparser` 同理

需要让 `CREATE TEXT SEARCH CONFIGURATION zhcfg USING zhparser` 这种 PG 标准用法不报错：
- 只兼容 zhparser 相关的 `CREATE TEXT SEARCH CONFIGURATION`（例如 `PARSER = zhparser` / `USING zhparser` 这种已知模式），并把 `zhcfg -> zhparser/jieba` 的映射 **持久化**（按租户 keyspace 隔离）
- `to_tsvector('zhcfg', text)` / `plainto_tsquery('zhcfg', text)` 通过该映射解析到 tokenizer（避免运行期“猜测 fallback”）
- 其他自定义 config DDL 返回 `0A000 feature_not_supported`（不要 silent accept）

**验收标准**：
```sql
-- 直接使用 zhparser 配置名
SELECT to_tsvector('zhparser', '分布式数据库');
-- 结果同 to_tsvector('chinese', '分布式数据库')

-- 兼容 CREATE EXTENSION 语法
CREATE EXTENSION IF NOT EXISTS zhparser;  -- 不报错

-- GIN 索引使用 zhparser
CREATE INDEX idx ON articles USING gin (to_tsvector('zhparser', content));
SELECT * FROM articles
WHERE to_tsvector('zhparser', content) @@ plainto_tsquery('zhparser', '数据库');
```

### Task 2.3: English Stemming (Snowball)

**依赖**: `rust-stemmers` crate (Snowball stemming for 14 languages)

**文件**: `src/sql/fts_tokenizers.rs`

```rust
fn tokenize_english_stemmed(text: &str) -> Vec<String> {
    use rust_stemmers::{Algorithm, Stemmer};
    let stemmer = Stemmer::create(Algorithm::English);
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .filter(|s| !is_english_stopword(s))  // 扩展后的 stopword 列表
        .map(|s| stemmer.stem(s).to_string())
        .collect()
}
```

**Stopword 列表扩展** (`src/sql/fts_stopwords.rs`, 新文件):
- 内嵌 PG 的 `english` stopword 列表（~174 词）
- 使用 `HashSet<&'static str>` + `OnceLock` 初始化
- `is_english_stopword()` 替代当前的 `is_simple_stopword()`

**配置更新**:
- `"english"` → 切换到 `tokenize_english_stemmed`（breaking change, 但更符合 PG 行为）
- `"simple"` → 保持当前 `tokenize_simple`（PG 的 simple config 也不做 stemming）

**验收标准**：
```sql
SELECT to_tsvector('english', 'The running dogs are happy');
-- 'run':2 'dog':3 'happi':5  （stemmed, stopwords removed）

SELECT to_tsvector('english', 'running') @@ to_tsquery('english', 'run');
-- true

SELECT to_tsvector('simple', 'running') @@ to_tsquery('simple', 'run');
-- false（simple 不做 stemming，与 PG 一致）
```

### Task 2.4: 混合 tokenizer — zhparser + ngram 组合策略

**目标**：为 CJK 提供最佳体验 —— jieba 精确分词 + ngram 兜底。

**新增配置**: `"chinese_ngram"` / `"zhparser_ngram"`

```rust
fn tokenize_chinese_ngram(text: &str) -> Vec<String> {
    let mut tokens = tokenize_jieba(text);
    // 对每个 jieba 分出的 CJK 多字词，额外产出 bigram
    let mut extra = Vec::new();
    for token in &tokens {
        let chars: Vec<char> = token.chars().collect();
        if chars.len() > 1 && chars.iter().any(|c| is_cjk_char(*c)) {
            for window in chars.windows(2) {
                extra.push(window.iter().collect::<String>());
            }
        }
    }
    tokens.extend(extra);
    tokens
}
```

**验收标准**：
```sql
SELECT to_tsvector('chinese_ngram', '分布式数据库');
-- 包含: '分布式', '数据库' (jieba), '分布', '布式', '数据', '据库' (bigram)

SELECT to_tsvector('chinese_ngram', '分布式数据库')
    @@ plainto_tsquery('chinese_ngram', '数据');
-- true（'数据' 在 bigram 中）
```

---

## Phase 3 — FTS 函数补全（达到主流使用体验）

**目标**：实现 `phraseto_tsquery`、`websearch_to_tsquery`、`ts_headline` 等应用层常用函数。
**预估工作量**：5-6 天

### Task 3.1: `phraseto_tsquery()` + `<->` 短语运算符

**文件**: `src/sql/fts.rs`

实现 `phraseto_tsquery(config?, text)`：
```sql
SELECT phraseto_tsquery('english', 'distributed database');
-- 结果: 'distribut' <-> 'databas'  (stemmed + phrase operator)
```

实现 `<->` (FOLLOWED BY) 运算符支持：
1. `TsQueryToken` 枚举新增 `FollowedBy(usize)` 变体（`<->` = distance 1, `<2>` = distance 2）
2. `tokenize_tsquery()` 解析 `<->` 和 `<N>` 语法
3. `match_tsquery()` 中利用 tsvector 的位置信息做距离判断

**关键修改**:
- `extract_tsvector_words()` 需要改为 `extract_tsvector_words_with_positions()` 返回 `HashMap<String, Vec<usize>>`
- 匹配逻辑：对 `A <-> B`，检查是否存在 `pos(A) + 1 == pos(B)`

**验收标准**：
```sql
SELECT to_tsvector('distributed database systems')
    @@ phraseto_tsquery('distributed database');
-- true

SELECT to_tsvector('database distributed systems')
    @@ phraseto_tsquery('distributed database');
-- false（顺序不对）

SELECT to_tsvector('distributed key value database')
    @@ to_tsquery('distributed <3> database');
-- true（距离 <= 3）
```

### Task 3.2: `websearch_to_tsquery()`

**文件**: `src/sql/fts.rs`

语法解析规则（与 PG 11+ 一致）：
- 无引号的词 → AND 连接
- `"quoted phrase"` → `<->` 连接
- `-term` → NOT
- `or` / `OR` → OR 连接
- 多余空白忽略

```
"distributed database" -mysql or postgres
→ 'distribut' <-> 'databas' & !'mysql' | 'postgr'
```

**注册**:
- `src/sql/expr/functions/fts.rs`: `map.insert("WEBSEARCH_TO_TSQUERY", websearch_to_tsquery);`
- `src/sql/types/registry/misc.rs`: 注册签名 `(Tsquery, 1..2 args)`

**验收标准**：
```sql
SELECT websearch_to_tsquery('english', '"quick fox" -lazy or dog');
-- 'quick' <-> 'fox' & !'lazi' | 'dog'

SELECT to_tsvector('the quick brown fox')
    @@ websearch_to_tsquery('"quick brown"');
-- true
```

### Task 3.3: `ts_headline()` 搜索高亮

**文件**: `src/sql/fts.rs` (新增) + `src/sql/expr/functions/fts.rs` (注册)

```sql
ts_headline(config?, document, query [, options])
```

实现逻辑：
1. 对 document 文本分词
2. 找到与 query 匹配的词的位置
3. 提取包含匹配词的文本片段（上下文窗口）
4. 用 `StartSel` / `StopSel` 标签包裹匹配词

**支持的选项**（options 字符串）：
- `StartSel` (default: `<b>`)
- `StopSel` (default: `</b>`)
- `MaxWords` (default: 35)
- `MinWords` (default: 15)
- `MaxFragments` (default: 0 = 整个文档)
- `FragmentDelimiter` (default: ` ... `)

**注册**:
- `src/sql/expr/functions/fts.rs`: `map.insert("TS_HEADLINE", ts_headline);`
- `src/sql/types/registry/misc.rs`: 签名 `(Text, 2..4 args)`

**验收标准**：
```sql
SELECT ts_headline('english',
    'The quick brown fox jumps over the lazy dog',
    plainto_tsquery('fox dog'));
-- 'The quick brown <b>fox</b> jumps over the lazy <b>dog</b>'

SELECT ts_headline('chinese',
    '分布式数据库是现代互联网架构的核心组件',
    plainto_tsquery('chinese', '数据库'),
    'StartSel=【, StopSel=】');
-- '分布式【数据库】是现代互联网架构的核心组件'
```

### Task 3.4: 改进 `ts_rank` / `ts_rank_cd`

**文件**: `src/sql/fts.rs`

`ts_rank` 改进：
- 实际支持 weights 参数：`ts_rank('{0.1, 0.2, 0.4, 1.0}', tsvector, tsquery)`
- 支持 normalization 参数（位掩码）：
  - `0` = 忽略文档长度
  - `1` = 除以 `1 + log(document_length)`
  - `2` = 除以 document_length
  - etc.
- 使用 tsvector 的 weight 信息（A/B/C/D）参与计算

`ts_rank_cd` 实现真正的 Cover Density Ranking：
- 计算查询词在文档中的覆盖密度
- 覆盖范围越窄（词越集中），分数越高

**验收标准**：
```sql
-- 权重影响排序
SELECT ts_rank(
    setweight(to_tsvector('title text'), 'A') ||
    setweight(to_tsvector('body text with title'), 'D'),
    plainto_tsquery('title')
) > ts_rank(
    setweight(to_tsvector('other text'), 'A') ||
    setweight(to_tsvector('body text with title'), 'D'),
    plainto_tsquery('title')
);
-- true (weight A 匹配比 weight D 分数高)
```

---

## Phase 4 — `LIKE '%...%'` 索引加速 (pg_trgm)

**目标**：`WHERE col LIKE '%关键词%'` / `WHERE col ILIKE '%keyword%'` 可利用 GIN trigram 索引。
**预估工作量**：4-5 天

### Task 4.1: Trigram Token 提取器

**新文件**: `src/sql/trgm.rs`

```rust
/// Extract trigrams from a string (pg_trgm algorithm).
/// Pads with 2 leading + 2 trailing spaces (pg_trgm boundary semantics).
pub fn extract_trigrams(text: &str) -> Vec<String> {
    // NOTE: 这里只是示意。pg_trgm 会按“词”切分，并对每个词左右各补 2 个空格：
    // 'cat' -> "  cat  " -> "  c" " ca" "cat" "at " "t  "
    let padded = format!("  {}  ", text.to_lowercase());
    let chars: Vec<char> = padded.chars().collect();
    chars.windows(3)
        .map(|w| w.iter().collect::<String>())
        .collect()
}

/// Extract trigrams from a LIKE pattern for index scan.
/// Extracts trigrams only from literal portions (outside % and _).
pub fn extract_trigrams_from_like(pattern: &str) -> Vec<String> { ... }
```

### Task 4.2: GIN trigram 索引支持

**修改**: `src/sql/gin.rs`

- `GinColumnType` 新增 `Trigram` 变体
- `supported_gin_index_column()` 识别 `USING gin (col gin_trgm_ops)` / `USING gin (col)` when opclass = trgm
- `extract_gin_token_hashes_from_row()` 新增 trigram hash 路径

**索引创建语法**:
```sql
CREATE INDEX idx_title_trgm ON articles USING gin (title gin_trgm_ops);
```

### Task 4.3: Planner 识别 LIKE/ILIKE → GIN trigram

**修改**: `src/sql/planner/index_selection.rs`

识别的谓词模式：
- `col LIKE '%pattern%'` → 从 pattern 提取 trigrams → GIN scan
- `col ILIKE '%pattern%'` → 同上（case-insensitive trigram）
- `col SIMILAR TO 'regex'` → 从 regex 提取固定部分的 trigrams
- `col ~ 'regex'` → 同上

> NOTE: pg_trgm 只有在“能提取出足够的 trigram（一般 pattern 长度 >= 3 且有足够 literal 部分）”时才值得走索引；同时 LIKE/regex 都需要 `Recheck Cond`（trigram 过滤是 lossy，会有 false positive）。

### Task 4.4: similarity() 函数族

**新文件**: `src/sql/expr/functions/trgm.rs`

```sql
similarity(text, text) → float4          -- trigram similarity (0..1)
word_similarity(text, text) → float4     -- word-level similarity
strict_word_similarity(text, text) → float4
show_trgm(text) → text[]                 -- show trigrams
```

**相关运算符**:
- `%`  (similarity threshold)
- `<%` (word_similarity threshold)
- `%>` / `<<%` / `%>>` etc.

**验收标准**：
```sql
CREATE INDEX idx ON articles USING gin (title gin_trgm_ops);

-- LIKE 利用 trigram 索引
EXPLAIN SELECT * FROM articles WHERE title LIKE '%数据库%';
-- → GIN Index Scan

-- similarity 函数
SELECT similarity('word', 'words');
-- 0.5 (or similar)
```

---

## Phase 5 — Fuzzy Matching + 兼容性

**目标**：补全 fuzzystrmatch 函数、catalog 虚拟表、tsvector trigger。
**预估工作量**：3-4 天

### Task 5.1: fuzzystrmatch 函数族

**新文件**: `src/sql/expr/functions/fuzzy.rs`

使用 `strsim` crate 或内置实现：

```sql
levenshtein(source, target) → int
levenshtein_less_equal(source, target, max_d) → int
damerau_levenshtein(source, target) → int
soundex(text) → text
difference(text, text) → int          -- soundex 差异
metaphone(text, max_output_length) → text
dmetaphone(text) → text              -- double metaphone
```

- 注册: `src/sql/expr/functions/fuzzy.rs` 注册所有函数
- 签名: `src/sql/types/registry/misc.rs`
- 兼容: `CREATE EXTENSION fuzzystrmatch` → no-op（内置）

### Task 5.2: `get_current_ts_config()` 函数

**文件**: `src/sql/expr/functions/fts.rs`

```rust
fn get_current_ts_config(args: Vec<Value>) -> Result<Value> {
    let config = /* 从当前 session 读取 default_text_search_config */;
    Ok(Value::Text(config.to_string()))
}
```

### Task 5.3: pg_catalog FTS 虚拟表 (基本兼容)

**新文件**: `src/sql/catalog/pg_ts_config.rs`

最小 viable 实现：返回硬编码的内置配置。

```sql
SELECT * FROM pg_ts_config;
-- cfgname | cfgnamespace | cfgowner | cfgparser
-- simple  | 11           | 10       | 3722
-- english | 11           | 10       | 3722
-- chinese | 11           | 10       | 3722
```

同理 `pg_ts_dict`, `pg_ts_parser` — 返回最小兼容数据使 ORM introspection 不报错。

### Task 5.4: `tsvector_update_trigger()` (可选)

**文件**: `src/sql/triggers/cache.rs` + `src/sql/fts.rs`

允许用户创建自动维护 tsvector 列的 trigger：
```sql
CREATE TRIGGER tsvector_update BEFORE INSERT OR UPDATE
ON documents FOR EACH ROW EXECUTE FUNCTION
tsvector_update_trigger(search_vector, 'chinese', title, body);
```

实现：在 trigger body 编译时识别 `tsvector_update_trigger`，生成等价的 `NEW.search_vector = to_tsvector(config, NEW.col1 || ' ' || NEW.col2)` 逻辑。

---

## 依赖关系

```
Phase 1 (GIN Scan)          ← 无依赖，可立即开始
    │
Phase 2 (Tokenizer)         ← 可与 Phase 1 并行（tokenizer 不依赖 GIN scan）
    │                            但 Phase 2 的效果需 Phase 1 完成后才能体现
    │
Phase 3 (FTS 函数)          ← 依赖 Phase 2（phrase search 需位置信息）
    │
Phase 4 (pg_trgm)           ← 依赖 Phase 1（复用 GIN scan 基础设施）
    │
Phase 5 (兼容性)            ← 可独立进行
```

## Cargo.toml 新增依赖

```toml
rust-stemmers = "1.2"    # Snowball stemming (Phase 2.3)
strsim = "0.11"          # String similarity (Phase 5.1, optional — 可内置)
```

> `jieba-rs` 已在依赖中。

## 测试策略

每个 Phase 完成后：
1. 新增对应的 `tests/xxx_*.sql` + `.expected` / `.assert` 文件
2. **必须**对照真实 PostgreSQL 17 验证 expected output（extension 相关能力需在 PG 侧启用对应 extension：`pg_trgm` / `fuzzystrmatch` / zhparser）
3. `cargo test` 通过（单元测试）
4. `python3 scripts/integration_test.py` 通过（集成测试）
5. 现有 FTS 测试不 regress：`tests/130_fts.sql`, `tests/131_gin_fts.sql`, `tests/218_gin_correctness.sql`, `tests/219_gin_chinese_fts.sql`

## 风险和注意事项

1. **GIN Scan 内存**: 当 token 匹配的 PK 集很大时（高频词如"的"），集合运算可能消耗大量内存。需要用流式集合运算 + 分批 `batch_get` 限制内存；如果仍可能超限，应在 planner 侧选择 Seq Scan 或显式报错（不要运行时静默换计划）。
2. **Stemming 是 breaking change**: `english` 配置启用 stemming 后，已有 GIN 索引的 token 不兼容。需要 **reindex** 或使用新配置名 `english_stem`。建议策略：保持 `english` = simple 不变，新增 `english_stem` 带 stemming，未来版本再切换默认。
3. **jieba 初始化慢**: jieba-rs 首次初始化需要 ~100ms 加载词典。已用 `OnceLock` 缓存，无额外问题。
4. **n-gram 索引膨胀**: n-gram 会产生比词级分词多得多的 token。10 字的中文文本，jieba 产出 ~5 个 token，bigram 产出 ~9 个。需要在文档中说明存储权衡。
5. **regconfig 类型**: PG 的 `to_tsvector(regconfig, text)` 第一个参数是 `regconfig` 类型不是 `text`。当前实现用 `text` 足够，但 ORM 生成的 DDL 可能包含 `::regconfig` cast，需要确保 cast 不报错。
