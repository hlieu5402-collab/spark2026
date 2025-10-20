# 指标契约 v1.0

> 目标：为 Spark 数据平面的核心调用链（Service / Codec / Transport）给出统一的指标命名、单位、稳定标签以及代码层面的打点挂钩。本文档配套 `alerts.yaml` 与 `dashboard.json`，可直接导入 Prometheus / Grafana 进行验收。

## 1. 命名与单位规范

- **命名规则**：`spark.<域>.<实体>.<指标>`，全部使用蛇形命名；域包含 `request`、`codec`、`transport`、`bytes` 等核心类别。
- **单位约定**：
  - 时间使用毫秒（`ms`）。
  - 字节量使用二进制字节（`bytes`）。
  - 计数器统一为“次/个”（`requests`、`connections` 等）。
- **稳定标签**：所有标签均限定在下列集合内，并在代码中以常量暴露，便于编译期审计。
  - `service.name`、`route.id`、`operation`、`protocol`、`status.code`、`outcome`
  - `codec.name`、`codec.mode`（`encode`/`decode`）、`content.type`、`error.kind`
  - `transport.protocol`、`socket.family`、`listener.id`、`peer.role`（`client`/`server`）、`result`
  - `instance`（由部署平台注入，用于区分副本，必须控制在 ≤1000/实例）

> **基数控制提示**：所有标签值须使用枚举或有限集，不得注入用户 ID、请求 ID 等高基数信息；如确需调试，请额外开辟临时指标或启用采样。

## 2. 核心指标最小必报集

| 域 | 指标名称 | 类型 / 单位 | 说明 | 必备标签 |
| --- | --- | --- | --- | --- |
| Service | `spark.request.total` | Counter (`requests`) | 成功+失败调用总次数 | `service.name` `route.id` `operation` `protocol` `outcome` |
| Service | `spark.request.duration` | Histogram (`ms`) | 单次调用端到端耗时 | `service.name` `route.id` `operation` `protocol` `status.code` `outcome` |
| Service | `spark.request.inflight` | Gauge (`requests`) | 并发中的调用数 | `service.name` `route.id` `operation` `protocol` |
| Service | `spark.request.errors` | Counter (`requests`) | 失败调用次数 | `service.name` `route.id` `operation` `protocol` `error.kind` |
| Service | `spark.bytes.inbound` / `spark.bytes.outbound` | Counter (`bytes`) | 请求/响应字节量 | `service.name` `route.id` `operation` `protocol` |
| Codec | `spark.codec.encode.duration` / `spark.codec.decode.duration` | Histogram (`ms`) | 编解码耗时 | `codec.name` `codec.mode` `content.type` |
| Codec | `spark.codec.encode.bytes` / `spark.codec.decode.bytes` | Counter (`bytes`) | 编解码后的字节量 | `codec.name` `codec.mode` `content.type` |
| Codec | `spark.codec.encode.errors` / `spark.codec.decode.errors` | Counter (`errors`) | 编解码失败次数 | `codec.name` `codec.mode` `content.type` `error.kind` |
| Transport | `spark.transport.connections` | Gauge (`connections`) | 当前活跃连接数 | `transport.protocol` `listener.id` `peer.role` |
| Transport | `spark.transport.connection.attempts` | Counter (`connections`) | 接入/拨号尝试次数 | `transport.protocol` `listener.id` `peer.role` `result` |
| Transport | `spark.transport.connection.failures` | Counter (`connections`) | 建连失败次数 | `transport.protocol` `listener.id` `peer.role` `error.kind` |
| Transport | `spark.transport.handshake.duration` | Histogram (`ms`) | 握手耗时 | `transport.protocol` `listener.id` `peer.role` `result` |
| Transport | `spark.transport.bytes.inbound` / `spark.transport.bytes.outbound` | Counter (`bytes`) | 物理链路上的收发字节 | `transport.protocol` `listener.id` `peer.role` |
| Limits | `spark.limits.usage` / `spark.limits.limit` | Gauge (`units`) | 资源当前占用与配置上限 | `limit.resource` `limit.action` |
| Limits | `spark.limits.hit` | Counter (`events`) | 资源达到限额的触发次数 | `limit.resource` `limit.action` |
| Limits | `spark.limits.drop` / `spark.limits.degrade` | Counter (`events`) | 超限后的拒绝 / 降级次数 | `limit.resource` `limit.action` |
| Limits | `spark.limits.queue.depth` | Gauge (`entries`) | 排队策略下的即时队列长度 | `limit.resource` `limit.action` |

## 3. 代码挂钩与最佳实践

`spark-core` 在以下模块中提供了开箱即用的打点辅助：

- `service::metrics::ServiceMetricsHook`
- `codec::metrics::CodecMetricsHook`
- `transport::metrics::TransportMetricsHook`

示例：在对象层 Service 实现中记录调用生命周期指标：

```rust
use spark_core::{
    service::metrics::{ServiceMetricsHook, ServiceOutcome, PayloadDirection},
    observability::OwnedAttributeSet,
};

fn handle_request(
    metrics: &dyn MetricsProvider,
    base_labels: OwnedAttributeSet,
) -> Result<Response, SparkError> {
    let hook = ServiceMetricsHook::new(metrics);
    let base = base_labels.as_slice();
    hook.on_call_start(base);

    let start = Instant::now();
    let response = do_business_logic()?;
    let duration = start.elapsed();

    let mut completion = base_labels.clone();
    completion.push_owned("status.code", "200");
    completion.push_owned("outcome", ServiceOutcome::SUCCESS_LABEL);
    hook.on_call_finish(base, ServiceOutcome::Success, duration, completion.as_slice());

    hook.record_payload_bytes(PayloadDirection::Outbound, response.len() as u64, base);
    Ok(response)
}
```

> **执行顺序**：先 `on_call_start` → 业务逻辑 → `on_call_finish`（无论成功或失败均需执行）→ 可选的 `record_payload_bytes`。

Codec 与 Transport 的钩子使用方式一致，分别面向 `EncodeContext`/`DecodeContext` 以及传输层连接管理逻辑。所有辅助函数均遵循**零 `Arc` 持有**策略，直接调用 `MetricsProvider::record_*`，避免额外堆分配。

## 4. 样例仪表盘与告警规则

- Prometheus 告警规则：见 [`alerts.yaml`](alerts.yaml)，通过 `promtool check rules docs/observability/alerts.yaml` 校验。
- Grafana 仪表盘：见 [`dashboard.json`](dashboard.json)，预置了服务成功率、P99 延迟、活跃连接等核心面板，默认变量 `service_name`/`listener_id`。

## 5. 验收指引

1. 导入上文指标契约，确保代码中仅使用列出的名称与标签；
2. 启动任意集成了 `ServiceMetricsHook` 的样例服务，可在 1 分钟内观察到 `spark.request.*` 与 `spark.transport.*` 指标数据；
3. 使用压测工具验证标签基数是否控制在 ≤1000（可通过 `count(count_over_time({__name__="spark.request.total"}[5m]))` 进行估算）；
4. `promtool check rules docs/observability/alerts.yaml` 必须返回 `SUCCESS`；
5. Grafana 导入 `dashboard.json` 后应能直接联动 `service_name`、`listener_id` 变量实现多实例对比。

> **后续计划**：v1.1 将补充 Streaming 指标、跨 Region 同步指标以及自动化 cardinality 保护策略。
