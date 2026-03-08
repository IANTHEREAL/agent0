# Auto Embedding 实施计划（TiDB 风格）

> 参照 TiDB Auto Embedding 方案，基于 `GENERATED ALWAYS AS (EMBED_TEXT(...)) STORED` 实现。
> 创建日期：2026-03-07

## 设计参考

- **TiDB Auto Embedding**: https://docs.pingcap.com/zh/ai/vector-search-auto-embedding-overview/
- **pgai Vectorizer** (Timescale): https://github.com/timescale/pgai — 异步方案，架构更重
- **选择 TiDB 方案的原因**: 复用标准 SQL `GENERATED ALWAYS AS ... STORED` 语义，无需自定义 DDL，代码量少，且 Stored Generated Column 本身是通用 SQL 功能

## 目标用户体验

```sql
-- 1. 创建带 auto-embedding 的表
CREATE TABLE documents (
    id SERIAL PRIMARY KEY,
    content TEXT,
    content_vector VECTOR(1024) GENERATED ALWAYS AS (
        EMBED_TEXT('bedrock/amazon-titan-v2', content)
    ) STORED
);

-- 2. 插入文本，向量自动生成
INSERT INTO documents (content) VALUES ('Electric vehicles reduce air pollution.');

-- 3. 用文本查询，自动 embed 查询文本
SELECT id, content FROM documents
ORDER BY VEC_EMBED_COSINE_DISTANCE(content_vector, 'renewable energy solutions')
LIMIT 10;
```

## 现状诊断

| 组件 | 现状 | 影响 |
|---|---|---|
| **Stored Generated Column** | ❌ 未实现。`create_table.rs` 只处理 `generation_expr: None`（identity/SERIAL） | 需要先实现此标准 SQL 功能 |
| **ColumnDef** | 无 `generation_expr` 字段，只有 `default_expr: Option<String>` | 需要扩展 model |
| **IMMUTABLE 检查** | 不存在（stored generated column 根本没实现） | 好消息——不需要"放宽"，从头设计 |
| **embedding() 函数** | 已有，锁定 OpenAI 兼容格式 + text-embedding-v4 | 需要 multi-provider |
| **距离函数** | `cosine_distance`, `l2_distance`, `inner_product` 已有 | 需加 `vec_embed_*` 变体 |
| **HNSW 规划器** | `hnsw_predicate.rs` 通过 `map_distance_metric()` 识别函数名 | 需扩展识别新函数名 |
| **EmbeddingConfig** | 全局 `OnceLock`，从环境变量初始化，无 provider 抽象 | 需要 provider 分发 |

## Bedrock 端点信息

```
Endpoint: https://bedrock-runtime.<region>.amazonaws.com/model/arn:aws:bedrock:<region>:<account-id>:application-inference-profile/<profile-id>/invoke
Auth: Bearer <your-bedrock-api-key>
Request:  {"inputText": "abc"}
Response: {"embedding": [...], "inputTextTokenCount": N}
```

与当前 OpenAI 兼容格式的差异：

| | 当前 (OpenAI 兼容) | Bedrock (Titan) |
|---|---|---|
| Request | `{"model":"...","input":"abc","dimensions":1024,"encoding_format":"float"}` | `{"inputText":"abc","dimensions":1024,"normalize":true}` |
| Response | `{"data":[{"embedding":[...]}],"usage":{"total_tokens":N}}` | `{"embedding":[...],"inputTextTokenCount":N}` |
| Model 指定 | request body | URL ARN |
| Endpoint 后缀 | 追加 `/embeddings` | 不追加（已包含 `/invoke`） |

---

## Phase 0: Multi-Provider Embedding Backend

**目标**：让 `call_embedding_api` 支持 Bedrock 端点。

**改动文件**：`src/config.rs`, `src/extensions/embedding.rs`, `src/sql/expr/functions/embedding.rs`

### 0.1 — EmbeddingConfig 扩展

```rust
// src/config.rs
#[derive(Debug, Clone)]
pub enum EmbeddingProvider {
    OpenAICompatible,  // 当前 DashScope / OpenAI / Azure
    Bedrock,           // AWS Bedrock (Titan 格式)
}

pub struct EmbeddingConfig {
    pub provider: EmbeddingProvider,  // 新增
    pub api_key: Option<String>,
    pub endpoint: String,
    pub model: String,
    pub dimensions: u32,
}
```

新增环境变量：`EMBEDDING_PROVIDER`（默认 `openai`，可选 `bedrock`）。

### 0.2 — Provider 分发

```rust
// src/extensions/embedding.rs
pub async fn call_embedding_api(text: &str, model: &str, dimensions: u32) -> Result<(Vec<f64>, u64)> {
    let config = get_embedding_config();
    match config.provider {
        EmbeddingProvider::OpenAICompatible => call_openai_compatible(config, text, model, dimensions).await,
        EmbeddingProvider::Bedrock => call_bedrock(config, text, dimensions).await,
    }
}

async fn call_bedrock(config: &EmbeddingConfig, text: &str, dimensions: u32) -> Result<(Vec<f64>, u64)> {
    let body = serde_json::json!({
        "inputText": text,
        "dimensions": dimensions,
        "normalize": true
    });
    // POST to config.endpoint (不追加 /embeddings)
    // Parse: json["embedding"] -> Vec<f64>, json["inputTextTokenCount"] -> u64
}
```

### 0.3 — 放宽模型限制

`canonical_embedding_model()` 改造：
- `provider=openai` → 保持当前 `text-embedding-v4` 限制
- `provider=bedrock` → 跳过模型校验（模型在 URL ARN 里）

### 0.4 — Endpoint 规范化

`normalize_embedding_endpoint()` 改造：
- `provider=openai` → 保持追加 `/embeddings` 的行为
- `provider=bedrock` → 不追加（Bedrock URL 已包含 `/invoke`）

### 0.5 — 测试

- Bedrock provider request/response JSON mock 单元测试
- 现有 OpenAI provider 回归测试不变
- `#[ignore]` 的 live Bedrock 集成测试

**预估**：~300 行，1-2 天

---

## Phase 1: Stored Generated Column 基础设施

**目标**：实现标准 SQL 的 `GENERATED ALWAYS AS (expr) STORED`。

这是一个通用 SQL 功能（PG、MySQL、TiDB 都支持），不限于 embedding。

### 1.1 — ColumnDef 扩展

```rust
// src/model/mod.rs
pub struct ColumnDef {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
    pub primary_key: bool,
    pub unique: bool,
    pub is_serial: bool,
    pub default_expr: Option<String>,
    #[serde(default)]
    pub generation_expr: Option<String>,  // 新增：GENERATED ALWAYS AS (expr) STORED
    #[serde(default)]
    pub collation: Option<String>,
}
```

`#[serde(default)]` 确保旧 schema 反序列化时此字段为 `None`（向后兼容）。

### 1.2 — DDL 解析（CREATE TABLE）

**文件**：`src/sql/ddl/create_table.rs`

当前 `ColumnOption::Generated` handler（~L127）只处理 `generation_expr: None`。新增分支：

```rust
ColumnOption::Generated {
    generated_as: GeneratedAs::Always,
    generation_expr: Some(expr),
    generation_expr_mode: Some(StoredOrVirtual::Stored),  // 只支持 STORED
    ..
} => {
    generation_expr_str = Some(expr.to_string());
    // 验证：
    // - generated column 不能同时有 DEFAULT
    // - generated column 不能是 PRIMARY KEY
    // - generated column 不能是 SERIAL
    // - 引用的源列必须存在于同表中
}
```

### 1.3 — DML：INSERT

**文件**：`src/sql/executor/dml_analyzed/insert.rs`

在 `fill_defaults_for_row` 之后（~L182 附近）：

```
for each column with generation_expr:
    1. 如果用户显式提供了该列的值 → 报错
       "ERROR: cannot insert a non-DEFAULT value into column \"col\"
        DETAIL: Column \"col\" is a generated column."
    2. 解析 generation_expr 为 SQL 表达式
    3. 用当前行的值替换表达式中的列引用
    4. 执行表达式 → 得到 Value
    5. 填入该列
```

**关键**：embedding() 内部使用 `tokio::task::block_in_place` 执行异步 HTTP，在 DML 路径中安全。

### 1.4 — DML：UPDATE

**文件**：`src/sql/executor/dml_analyzed/update.rs`

```
for each column with generation_expr:
    1. 如果用户显式 SET 了该列 → 报错
    2. 如果该列依赖的源列被更新了 → 重新计算
    3. 否则 → 保持原值
```

### 1.5 — DDL：ALTER TABLE

- `ALTER TABLE t ADD COLUMN v INT GENERATED ALWAYS AS (a + b) STORED` → 支持
- `ALTER TABLE t DROP COLUMN v` → 如果 v 被其他 generated column 引用，报错

**文件**：`src/sql/ddl/alter_table/columns.rs`

### 1.6 — Catalog 展示

**文件**：`src/sql/catalog/columns.rs`

`information_schema.columns` 的 `generation_expression` 列：从空字符串改为返回实际的 `generation_expr`。

### 1.7 — 分析器集成

**文件**：`src/sql/analyzer/dml.rs`

INSERT/UPDATE 分析时，识别 generated column 并标记为"不可直接赋值"。

### 1.8 — 测试

```sql
-- 基本功能
CREATE TABLE t (a INT, b INT GENERATED ALWAYS AS (a * 2) STORED);
INSERT INTO t (a) VALUES (5);
SELECT * FROM t; -- 期望: a=5, b=10

-- 更新联动
UPDATE t SET a = 10 WHERE a = 5;
SELECT * FROM t; -- 期望: a=10, b=20

-- 错误场景
INSERT INTO t (a, b) VALUES (5, 10); -- ERROR: cannot insert into generated column
UPDATE t SET b = 20; -- ERROR: cannot update generated column
```

**预估**：~600 行，2-3 天

---

## Phase 2: EMBED_TEXT + Auto Embedding

**目标**：在 Phase 1 基础上，实现 TiDB 风格的 auto-embedding。

### 2.1 — EMBED_TEXT 函数

**文件**：`src/sql/expr/functions/embedding.rs`

```rust
pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("EMBEDDING", embedding_fn);
    map.insert("EMBED_TEXT", embed_text_fn);  // 新增
}

/// EMBED_TEXT(model, text [, json_options])
/// TiDB 兼容语法：模型名为第一参数
fn embed_text_fn(args: Vec<Value>) -> Result<Value> {
    if args.len() < 2 || args.len() > 3 {
        return Err(...);
    }
    let model = match &args[0] {
        Value::Text(m) => m.clone(),
        _ => return Err(...),
    };
    let text = args[1].clone();
    let dimensions = if let Some(Value::Text(opts)) = args.get(2) {
        // 解析 JSON options: {"dimensions": 1024}
        parse_json_dimensions(opts)?
    } else {
        None
    };
    // 复用 embedding_fn 核心逻辑
    // ...
}
```

### 2.2 — 类型注册

**文件**：`src/sql/types/registry/` (misc.rs 或新建 vector.rs)

```
EMBED_TEXT(TEXT, TEXT) -> VECTOR
EMBED_TEXT(TEXT, TEXT, TEXT) -> VECTOR  -- 带 JSON options
```

### 2.3 — 端到端验证

```sql
-- Bedrock provider
SET embedding.provider = 'bedrock';
SET embedding.endpoint = 'https://bedrock-runtime...';
SET embedding.api_key = 'ABSK...';

CREATE TABLE documents (
    id SERIAL PRIMARY KEY,
    content TEXT,
    content_vector VECTOR(1024) GENERATED ALWAYS AS (
        EMBED_TEXT('bedrock/amazon-titan-v2', content)
    ) STORED
);

INSERT INTO documents (content) VALUES ('hello world');
SELECT id, content, vector_dims(content_vector) FROM documents;
-- 期望: id=1, content='hello world', vector_dims=1024
```

### 2.4 — 扩展门控

`embed_text_fn` 入口调用 `ensure_embedding_installed_gate()`（与 `embedding()` 一致）。

**预估**：~200 行，1 天

---

## Phase 3: Auto Query（VEC_EMBED_* 距离函数）

**目标**：查询时传入文本，自动 embed 再计算距离。

### 3.1 — 新增函数

**文件**：`src/sql/expr/functions/vector.rs`

```rust
pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    // 现有
    map.insert("L2_DISTANCE", l2_distance_fn);
    map.insert("COSINE_DISTANCE", cosine_distance_fn);
    map.insert("INNER_PRODUCT", inner_product_fn);
    // 新增
    map.insert("VEC_EMBED_L2_DISTANCE", vec_embed_l2_distance_fn);
    map.insert("VEC_EMBED_COSINE_DISTANCE", vec_embed_cosine_distance_fn);
}

pub fn vec_embed_cosine_distance_fn(args: Vec<Value>) -> Result<Value> {
    if args.len() != 2 {
        return Err(anyhow!("vec_embed_cosine_distance requires exactly 2 arguments"));
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    let vec1 = extract_vector(&args[0])?;
    let vec2 = match &args[1] {
        Value::Text(text) => {
            // 自动调用 embedding() 将文本转为向量
            let embedded = embedding_fn(vec![Value::Text(text.clone())])?;
            extract_vector(&embedded)?
        }
        other => extract_vector(other)?,
    };
    let dist = cosine_distance(&vec1, &vec2)?;
    Ok(Value::Float64(dist))
}
```

`vec_embed_l2_distance_fn` 同理。

### 3.2 — HNSW 规划器扩展

**文件**：`src/sql/planner/hnsw_predicate.rs`

```rust
fn map_distance_metric(func_name: &str) -> Option<&'static str> {
    if func_name.eq_ignore_ascii_case("l2_distance")
        || func_name.eq_ignore_ascii_case("vec_embed_l2_distance")
    {
        Some("l2")
    } else if func_name.eq_ignore_ascii_case("cosine_distance")
        || func_name.eq_ignore_ascii_case("vec_embed_cosine_distance")
    {
        Some("cosine")
    } else if func_name.eq_ignore_ascii_case("inner_product") {
        Some("ip")
    } else {
        None
    }
}
```

`extract_vector_col_and_constant_vector` 扩展：对 `vec_embed_*` 函数，第二参数为 Text 常量时，在 HNSW scan 构建阶段调用 `embedding()` 预转换为向量。

### 3.3 — 类型注册

```
VEC_EMBED_COSINE_DISTANCE(VECTOR, TEXT) -> FLOAT8
VEC_EMBED_L2_DISTANCE(VECTOR, TEXT) -> FLOAT8
```

### 3.4 — 测试

```sql
-- 基本功能
SELECT VEC_EMBED_COSINE_DISTANCE(content_vector, 'search text') FROM documents LIMIT 5;

-- HNSW 索引加速
CREATE INDEX ON documents USING hnsw (content_vector cosine);
EXPLAIN SELECT id FROM documents
ORDER BY VEC_EMBED_COSINE_DISTANCE(content_vector, 'search text')
LIMIT 5;
-- 期望: 走 HNSW Index Scan
```

**预估**：~300 行，1-2 天

---

## Phase 4: 外部配置完善

**目标**：GUC 动态配置 embedding provider。

### 4.1 — GUC 新增

**文件**：`src/sql/session/settings.rs`

```sql
SET embedding.provider = 'bedrock';
SET embedding.endpoint = 'https://bedrock-runtime...';
SET embedding.api_key = 'ABSK...';
SET embedding.model = 'amazon-titan-v2';
SET embedding.dimensions = '1024';

SHOW embedding.provider;
SHOW embedding.endpoint;
SHOW embedding.model;
```

- `SHOW embedding.api_key` → 返回 `'****'`（masked）
- 配置优先级：`Session SET > 环境变量 > 内置默认值`

### 4.2 — 动态配置读取

`embedding()` / `embed_text()` 函数中，每次调用时读取最新 GUC 值（已有 `setting_or` 模式），扩展到 provider 和 endpoint。

**预估**：~150 行，0.5 天

---

## 实施顺序与依赖

```
Phase 0 (Multi-Provider)           ← Bedrock 端点可用
    ↓
Phase 1 (Stored Generated Column)  ← 标准 SQL 功能，核心前置依赖
    ↓
Phase 2 (EMBED_TEXT + Auto Embed)  ← INSERT 时自动 embedding
    ↓
Phase 3 (VEC_EMBED_* Auto Query)   ← 查询时自动 embedding
    ↓
Phase 4 (GUC 配置)                 ← 动态配置
```

## 里程碑

| 里程碑 | 内容 | 验收标准 | 预估 |
|---|---|---|---|
| **M0** | Phase 0 | `SELECT embedding('hello')` 通过 Bedrock 端点返回向量 | 1-2 天 |
| **M1** | Phase 1 | `CREATE TABLE t (a INT, b INT GENERATED ALWAYS AS (a * 2) STORED); INSERT INTO t (a) VALUES (5);` → b=10 | 2-3 天 |
| **M2** | Phase 2 | `CREATE TABLE docs (..., vec VECTOR(1024) GENERATED ALWAYS AS (EMBED_TEXT(..., content)) STORED); INSERT INTO docs (content) VALUES ('hello');` → vec 自动填充 | 1 天 |
| **M3** | Phase 3 | `SELECT * FROM docs ORDER BY VEC_EMBED_COSINE_DISTANCE(vec, 'text') LIMIT 5;` → 正确返回且走 HNSW 索引 | 1-2 天 |
| **M4** | Phase 4 | `SET embedding.provider = 'bedrock';` 动态生效 | 0.5 天 |

**总预估**：5-8 天

## 文件改动清单

| Phase | 文件 | 改动类型 |
|---|---|---|
| 0 | `src/config.rs` | 修改：EmbeddingProvider enum, EmbeddingConfig 扩展 |
| 0 | `src/extensions/embedding.rs` | 修改：Provider 分发, Bedrock 实现 |
| 0 | `src/sql/expr/functions/embedding.rs` | 修改：放宽模型校验 |
| 1 | `src/model/mod.rs` | 修改：ColumnDef 加 generation_expr |
| 1 | `src/sql/ddl/create_table.rs` | 修改：解析 GENERATED ALWAYS AS (expr) STORED |
| 1 | `src/sql/ddl/alter_table/columns.rs` | 修改：ALTER TABLE ADD/DROP generated column |
| 1 | `src/sql/executor/dml_analyzed/insert.rs` | 修改：INSERT 时计算 generated column |
| 1 | `src/sql/executor/dml_analyzed/update.rs` | 修改：UPDATE 时重算 generated column |
| 1 | `src/sql/analyzer/dml.rs` | 修改：标记 generated column 不可直接赋值 |
| 1 | `src/sql/catalog/columns.rs` | 修改：展示 generation_expression |
| 2 | `src/sql/expr/functions/embedding.rs` | 修改：新增 EMBED_TEXT 函数 |
| 2 | `src/sql/types/registry/` | 修改：EMBED_TEXT 类型签名 |
| 3 | `src/sql/expr/functions/vector.rs` | 修改：新增 VEC_EMBED_* 函数 |
| 3 | `src/sql/planner/hnsw_predicate.rs` | 修改：识别新距离函数 |
| 3 | `src/sql/types/registry/` | 修改：VEC_EMBED_* 类型签名 |
| 4 | `src/sql/session/settings.rs` | 修改：新增 embedding.provider 等 GUC |

## 风险与注意事项

1. **INSERT 延迟**：embedding API 调用耗时 50-500ms，GENERATED column 是同步计算的，会阻塞 INSERT。对于批量 INSERT，延迟会叠加。可考虑后续增加 batch embedding 优化。

2. **API 失败**：如果 embedding API 不可用，INSERT 会失败。这是 TiDB 的相同行为（同步语义的固有特征）。用户需确保 API 可用性。

3. **Schema 序列化兼容**：`ColumnDef` 新增 `generation_expr` 字段用 `#[serde(default)]`，旧版本 schema 反序列化时自动为 None。

4. **HNSW 与 VEC_EMBED_***：HNSW scan 需要在构建阶段将 Text 查询预转换为 Vector，确保 embedding API 只调用一次（不是每行都调用）。

5. **Token 用量**：auto-embedding 会显著增加 token 消耗，需配合 `embedding.max_calls` 限制使用。

6. **事务语义**：如果 INSERT 在 embedding 之后、写入 TiKV 之前失败（如唯一约束冲突），embedding token 已经消耗但不可回滚。这与 TiDB 行为一致。
