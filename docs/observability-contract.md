# 可观测性契约键名规范

本文档描述了框架内默认可观测性契约（`DEFAULT_OBSERVABILITY_CONTRACT`）暴露的所有指标、日志字段、追踪键与审计字段。目标是保证文档、实现与消费方约定保持一致，避免键名漂移导致监控面混乱。

## 机器可校验的契约清单

```json
{
  "metric_names": [
    "spark.request.total",
    "spark.request.duration",
    "spark.request.inflight",
    "spark.request.errors",
    "spark.bytes.inbound",
    "spark.bytes.outbound",
    "spark.codec.encode.duration",
    "spark.codec.decode.duration",
    "spark.codec.encode.bytes",
    "spark.codec.decode.bytes",
    "spark.codec.encode.errors",
    "spark.codec.decode.errors",
    "spark.transport.connections",
    "spark.transport.connection.attempts",
    "spark.transport.connection.failures",
    "spark.transport.handshake.duration",
    "spark.transport.bytes.inbound",
    "spark.transport.bytes.outbound",
    "spark.limits.usage",
    "spark.limits.limit",
    "spark.limits.hit",
    "spark.limits.drop",
    "spark.limits.degrade",
    "spark.limits.queue.depth",
    "spark.pipeline.epoch",
    "spark.pipeline.mutation.total"
  ],
  "log_fields": [
    "request.id",
    "route.id",
    "caller.identity",
    "peer.identity",
    "budget.kind"
  ],
  "trace_keys": [
    "traceparent",
    "tracestate",
    "spark-budget"
  ],
  "audit_schema": [
    "event_id",
    "sequence",
    "occurred_at",
    "actor_id",
    "action",
    "entity_kind",
    "entity_id",
    "state_prev_hash",
    "state_curr_hash",
    "tsa_evidence"
  ]
}
```

> 上表仅用于 CI 校验，任何键名调整需同时更新代码与此处 JSON 列表。

## 指标键（`metric_names`）

| 键名 | 含义 | 示例 |
| --- | --- | --- |
| spark.request.total | 单位时间内接收到的请求数量。 | `spark.request.total{route="upload"} = 128` |
| spark.request.duration | 请求处理耗时分布（通常采用直方图/摘要）。 | `p95 = 42ms` |
| spark.request.inflight | 当前正在处理的请求数。 | `current = 8` |
| spark.request.errors | 失败请求数量，包含应用与协议错误。 | `spark.request.errors{code="APP_TIMEOUT"} = 3` |
| spark.bytes.inbound | 请求体/流量入站字节数。 | `bytes = 524288` |
| spark.bytes.outbound | 响应体/流量出站字节数。 | `bytes = 786432` |
| spark.codec.encode.duration | 编码阶段的耗时分布。 | `avg = 3.5ms` |
| spark.codec.decode.duration | 解码阶段的耗时分布。 | `avg = 4.1ms` |
| spark.codec.encode.bytes | 编码后输出的字节量。 | `bytes = 4096` |
| spark.codec.decode.bytes | 解码时读取的字节量。 | `bytes = 4096` |
| spark.codec.encode.errors | 编码阶段的错误次数。 | `count = 1` |
| spark.codec.decode.errors | 解码阶段的错误次数。 | `count = 2` |
| spark.transport.connections | 活跃传输连接数量。 | `connections = 12` |
| spark.transport.connection.attempts | 发起传输连接的尝试次数。 | `attempts = 5` |
| spark.transport.connection.failures | 连接失败次数。 | `failures = 2` |
| spark.transport.handshake.duration | 连接握手耗时分布。 | `p99 = 80ms` |
| spark.transport.bytes.inbound | 传输层入站字节数。 | `bytes = 65536` |
| spark.transport.bytes.outbound | 传输层出站字节数。 | `bytes = 98304` |
| spark.limits.usage | 当前资源占用量（如并发、速率等指标）。 | `usage = 75` |
| spark.limits.limit | 资源上限值。 | `limit = 100` |
| spark.limits.hit | 触发限流判定的次数。 | `hits = 4` |
| spark.limits.drop | 因限流被直接丢弃的请求数。 | `drops = 1` |
| spark.limits.degrade | 因限流而采取降级策略的次数。 | `degrade = 2` |
| spark.limits.queue.depth | 限流队列当前深度。 | `queue_depth = 16` |
| spark.pipeline.epoch | 当前管线配置的 epoch 版本号。 | `epoch = 42` |
| spark.pipeline.mutation.total | 动态管线变更累计次数。 | `mutations = 7` |

## 日志字段（`log_fields`）

| 键名 | 含义 | 示例 |
| --- | --- | --- |
| request.id | 全局唯一的请求标识，用于跨系统定位。 | `"req-20240318-abcdef"` |
| route.id | 命中的路由或 handler 标识。 | `"pipeline.upload"` |
| caller.identity | 调用方身份（例如应用或租户）。 | `"tenant_001"` |
| peer.identity | 对端身份（例如下游服务或代理）。 | `"spark-edge"` |
| budget.kind | 当前预算类型（如并发、速率、配额）。 | `"rate"` |

## 追踪键（`trace_keys`）

| 键名 | 含义 | 示例 |
| --- | --- | --- |
| traceparent | W3C Trace Context 标准的父追踪标识。 | `"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"` |
| tracestate | W3C Trace Context 的 vendor 扩展链。 | `"congo=t61rcWkgMzE"` |
| spark-budget | Spark 自定义预算传播头。 | `"concurrency=10,rate=200rps"` |

## 审计字段（`audit_schema`）

| 键名 | 含义 | 示例 |
| --- | --- | --- |
| event_id | 审计事件的唯一标识。 | `"audit-20240318-0001"` |
| sequence | 相同实体下的递增序列号。 | `42` |
| occurred_at | 事件发生的 UTC 时间戳。 | `"2024-03-18T12:00:00Z"` |
| actor_id | 触发事件的主体身份。 | `"svc-admin"` |
| action | 执行的动作类型。 | `"update_policy"` |
| entity_kind | 被操作实体的类型。 | `"policy"` |
| entity_id | 被操作实体的唯一标识。 | `"policy-123"` |
| state_prev_hash | 变更前实体状态的哈希。 | `"prev:sha256:..."` |
| state_curr_hash | 变更后实体状态的哈希。 | `"curr:sha256:..."` |
| tsa_evidence | 时间戳权威服务的佐证。 | `"tsa:20240318T120000Z:signature"` |

## 更新流程

1. 修改 `spark-core/src/contract.rs` 中的 `DEFAULT_OBSERVABILITY_CONTRACT`。
2. 同步更新本文档 JSON 列表与说明表格。
3. 运行 `bash tools/ci/check_observability_keys.sh` 确认 CI 校验通过。

遵循以上流程，可确保契约在文档、实现与各类数据面之间保持一致。
