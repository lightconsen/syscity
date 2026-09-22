# Multi-Query 设计文档

Multi-Query 检索增强：将用户 query 扩展为多个子查询并行检索，再合并结果。

## 1. 原理

单条 query 经过 embedding 后只覆盖一个"语义位置"。复合 query（如"怎么部署和监控"）可能落在部署和监控的中间地带，两头都不够近。

Multi-Query 解决这个问题：

```
用户: "怎么部署和监控这个服务"
                        │
          ┌─────────────┴─────────────┐
          │  LLM 扩展为 N 个子查询     │
          │                           │
    "部署步骤是什么"      "如何配置健康检查"      "告警规则怎么设"
          │                      │                      │
    ┌─────▼─────┐          ┌─────▼─────┐          ┌─────▼─────┐
    │ embedding  │          │ embedding  │          │ embedding  │
    │ + search   │          │ + search   │          │ + search   │
    └─────┬─────┘          └─────┬─────┘          └─────┬─────┘
          │                      │                      │
          └──────────────────────┼──────────────────────┘
                                 │
                    ┌────────────▼────────────┐
                    │  RRF 合并 (去重 + 重排)  │
                    └────────────┬────────────┘
                                 │
                           最终结果列表
```

## 2. 数据模型

### 核心逻辑（`src/rag/multi_query.rs`）

```rust
/// Multi-Query 配置
#[derive(Debug, Clone)]
pub struct MultiQueryConfig {
    /// 启用 Multi-Query
    pub enabled: bool,
    /// 生成的子查询数量（不包括原始 query），默认 3
    /// 总检索次数 = num_variations + 1（原始 query）
    pub num_variations: usize,
    /// 合并策略
    pub merge_strategy: MergeStrategy,
}

/// RRF (Reciprocal Rank Fusion) 合并策略参数
#[derive(Debug, Clone, Copy)]
pub struct RrfConfig {
    /// RRF 常数 k，默认 60
    pub k: usize,
}

#[derive(Debug, Clone)]
pub enum MergeStrategy {
    /// Reciprocal Rank Fusion — 基于排名的无参数合并
    Rrf(RrfConfig),
}

impl Default for MultiQueryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            num_variations: 3,
            merge_strategy: MergeStrategy::Rrf(RrfConfig { k: 60 }),
        }
    }
}
```

### 扩展函数

```rust
/// 用 LLM 将 query 扩展为多个子查询变体。
///
/// 返回包含原始 query 在内的 `num_variations + 1` 个查询。
pub fn expand_query(
    query: &str,
    num_variations: usize,
) -> Vec<String> {
    // 等会儿写 prompt
}
```

### 合并函数

```rust
/// 用 RRF 合并多个结果集。
///
/// RRF score = Σ 1 / (k + rank_i(d))
/// 其中 rank_i(d) 是文档 d 在第 i 个结果集中的排序位置（1-based）。
pub fn merge_results<T>(
    result_sets: Vec<Vec<(T, f32)>>,
    config: &RrfConfig,
) -> Vec<(T, f32)>
where
    T: Clone + Hash + Eq,  // 用 content 或 id 去重
{
    // 1. 每个结果集按原始 score 排序（search_similar 已排序）
    // 2. 对每个文档，遍历所有结果集：
    //    - 找到它在每个集中的 rank（1-based）
    //    - RRF score = Σ 1 / (k + rank)
    //    - 未出现的 rank = 无穷大（贡献为 0）
    // 3. 按 RRF score 降序排列
    // 4. 取 top_limit 个
}
```

## 3. LLM 调用

### Prompt

与 HyDE 一样，Multi-Query 需要一个 LLM provider 来生成 query 变体。

```rust
// src/rag/multi_query.rs

const EXPAND_PROMPT: &str = "\
You are a query expansion assistant. Given the user's original search query, \
generate {num} different versions that cover different aspects or phrasings.

Rules:
- Each version must be a complete, self-contained search query
- Cover different perspectives or terminology
- Do NOT number the queries — output one per line, no prefixes
- Keep each query concise (under 20 words)

Original query: {query}

Alternative queries:";
```

### 调用方式

复用 HyDE 已经建立的模式——LLM provider 在 gateway init 时注入：

```rust
// 在 VectorMemoryService 中
pub struct VectorMemoryService {
    // ... 现有字段 ...
    multi_query_provider: Option<Arc<dyn Provider>>,
    multi_query_config: Option<MultiQueryConfig>,
}

impl VectorMemoryService {
    pub fn with_multi_query(
        mut self,
        provider: Arc<dyn Provider>,
        config: MultiQueryConfig,
    ) -> Self {
        self.multi_query_provider = Some(provider);
        self.multi_query_config = Some(config);
        self
    }
}
```

注意：**HyDE 和 Multi-Query 是正交的**，可以同时启用：

```
Multi-Query 在外层（循环 N 个变体）
    └── 每个变体内：HyDE（假设文档）→ Embed → Search
```

## 4. 集成到 VectorMemoryService

### search() 和 search_collection() 的改动

```rust
// src/memory/vector.rs

pub async fn search(
    &self,
    query: &str,
    limit: usize,
    threshold: f32,
) -> crate::Result<Vec<(EmbeddedChunk, f32)>> {
    // ── Multi-Query 路径 ─────────────────────────────────
    if let Some(ref mq_config) = self.multi_query_config {
        if mq_config.enabled && mq_config.num_variations > 0 {
            return self.search_multi_query(query, limit, threshold, None, mq_config).await;
        }
    }

    // ── 原始路径 ─────────────────────────────────────────
    let rewritten = self.query_transformer.transform(query).await?;
    let query_embedding = self.embedding_provider.embed(&rewritten).await?;
    self.vector_store
        .search_similar(&query_embedding, limit, threshold, None)
        .await
}

pub async fn search_collection(
    &self,
    query: &str,
    limit: usize,
    collection: &str,
    threshold: f32,
) -> crate::Result<Vec<SearchResult>> {
    // ── Multi-Query 路径 ─────────────────────────────────
    if let Some(ref mq_config) = self.multi_query_config {
        if mq_config.enabled && mq_config.num_variations > 0 {
            let results = self.search_multi_query(
                query, limit, threshold, Some(collection), mq_config
            ).await?;
            return Ok(results.into_iter().map(|(chunk, score)| SearchResult {
                id: chunk.id,
                content: chunk.text,
                score,
                metadata: chunk.metadata,
            }).collect());
        }
    }

    // ── 原始路径 ─────────────────────────────────────────
    let rewritten = self.query_transformer.transform(query).await?;
    let query_embedding = self.embedding_provider.embed(&rewritten).await?;
    let results = self.vector_store
        .search_similar(&query_embedding, limit, threshold, Some(collection))
        .await?;
    Ok(results.into_iter().map(|(chunk, score)| SearchResult {
        id: chunk.id,
        content: chunk.text,
        score,
        metadata: chunk.metadata,
    }).collect())
}

/// Multi-Query 核心逻辑：扩展 → N 次检索 → RRF 合并
async fn search_multi_query(
    &self,
    query: &str,
    limit: usize,
    threshold: f32,
    collection: Option<&str>,
    mq_config: &MultiQueryConfig,
) -> crate::Result<Vec<(EmbeddedChunk, f32)>> {
    // 1. 用 LLM 扩展 query
    let provider = self.multi_query_provider.as_ref()
        .ok_or_else(|| /* error */)?;
    let sub_queries = expand_query_with_llm(
        query, mq_config.num_variations, provider
    ).await?;
    // sub_queries = [原始 query, 变体1, 变体2, ...]

    // 2. 并行检索每个变体
    let mut handles = Vec::new();
    for sub_q in &sub_queries {
        // 每个变体内走 QueryTransformer（HyDE）+ Embed + Search
        let rewritten = self.query_transformer.transform(sub_q).await?;
        let emb = self.embedding_provider.embed(&rewritten).await?;
        let results = self.vector_store
            .search_similar(&emb, limit, threshold, collection)
            .await?;
        handles.push(results);
    }

    // 3. RRF 合并
    let rrf_config = match mq_config.merge_strategy {
        MergeStrategy::Rrf(ref c) => c,
    };
    Ok(merge_results(handles, &rrf_config, limit))
}
```

## 5. 配置

### Gateway Config (`syscity.toml`)

```toml
[vector_memory]
enabled = true

# 已有
[vector_memory.query_transformer]
enable_hyde = true

# 新增
[vector_memory.multi_query]
enabled = true
num_variations = 3
```

### VectorMemoryConfig 新增字段

```rust
// src/gateway/config/memory.rs

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorMemoryConfig {
    // ... 现有字段 ...
    /// Query transformer configuration (HyDE)
    #[serde(default)]
    pub query_transformer: QueryTransformerConfig,
    /// Multi-Query expansion configuration
    #[serde(default)]
    pub multi_query: MultiQueryConfig,
    // ...
}

/// Multi-Query 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiQueryConfig {
    /// 启用 Multi-Query
    pub enabled: bool,
    /// 子查询变体数（不含原始 query），默认 3
    pub num_variations: usize,
}
```

## 6. Gateway Init 注入

与 HyDE 相同的模式，在 `services.rs` 中创建 `VectorMemoryService` 后注入：

```rust
// src/gateway/init/services.rs

// ── Multi-Query ──────────────────────────────────────
let mqc = &config.vector_memory.multi_query;
if mqc.enabled && mqc.num_variations > 0 {
    match state.infra.model_router.create_default_provider().await {
        Ok(provider) => {
            let mq_config = crate::rag::multi_query::MultiQueryConfig {
                enabled: true,
                num_variations: mqc.num_variations,
                ..Default::default()
            };
            service = service.with_multi_query(provider, mq_config);
            info!("Multi-Query enabled with {} variations", mqc.num_variations);
        }
        Err(e) => {
            warn!("Failed to create LLM provider for Multi-Query: {}", e);
        }
    }
}
```

## 7. 文件变更清单

| 文件 | 操作 | 说明 |
|------|------|------|
| `src/rag/multi_query.rs` | **新增** | `expand_query()` + `merge_results()` + `MultiQueryConfig` |
| `src/rag/mod.rs` | 修改 | 添加 `pub mod multi_query;` |
| `src/memory/vector.rs` | 修改 | 添加 `multi_query_provider`、`multi_query_config` 字段、`with_multi_query()`、`search_multi_query()` |
| `src/gateway/config/memory.rs` | 修改 | `VectorMemoryConfig` 添加 `multi_query: MultiQueryConfig` 字段 |
| `src/gateway/init/services.rs` | 修改 | 注入 Multi-Query provider |

## 8. 注意事项

### 性能

- 总检索次数 = `num_variations + 1`。默认 3 变体 = 4 次检索
- embedding 调用次数 = 同样次数（但 embedding 是轻量操作）
- 4 次并行检索对 SQLite 来说可忽略——`tokio::join!` 跑多个 `search_similar` 是并发的 I/O

### 与 HyDE 的交互

两个增强是**串联**的：

```
Multi-Query 外层循环:
  for sub_q in [query, var1, var2]:
      hyde = query_transformer.transform(sub_q)  ← HyDE 在内部
      emb = embed(hyde)
      search(emb)
```

LLM 调用次数 = variations * (1 HyDE + 1 expand)。如果 HyDE 也开着，就是 4 * 2 = 8 次 LLM 调用。建议用 haiku 或同级别快速模型。

### 与 Reranker 的交互

Multi-Query 合并后得到 N 个结果，之后仍然走现有的 reranker → context budget 链路，不需要额外改动。

### 与 KB Collection 的交互

`search_collection()` 同样走 Multi-Query 路径，只需把 `collection` 参数透传给 `search_multi_query()`。一次实现，memory 和 KB 都受益。

## 9. 测试

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rrf_merge_basic() {
        // 两个结果集，验证 RRF 合并后的排序
    }

    #[test]
    fn test_rrf_merge_empty() {
        // 空结果集
    }

    #[test]
    fn test_rrf_merge_single_set() {
        // 只有一个结果集时等同于透传
    }

    #[test]
    fn test_rrf_k_value_effect() {
        // 不同 k 值对排序的影响
    }
}
```
