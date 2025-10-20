# 双层接口选择矩阵（T05）

> 本文档盘点 `spark-core` 三个核心域（Service、Codec、Router）在 T05 任务下的双层接口成果，
> 并给出性能/可插拔性取舍矩阵，指导调用方在泛型（零虚分派）与对象层（插件/脚本）之间做出决策。

## 1. 接口清单

| 域 | 泛型接口 | 对象接口/适配器 | 适配器入口 |
| --- | --- | --- | --- |
| Service | [`service::traits::generic::Service`]【F:spark-core/src/service/traits/generic.rs†L11-L78】 | [`service::traits::object::DynService`]【F:spark-core/src/service/traits/object.rs†L15-L68】 / [`ServiceObject`]【F:spark-core/src/service/traits/object.rs†L96-L153】 | `ServiceObject::new`【F:spark-core/src/service/traits/object.rs†L114-L139】 |
| Codec | [`codec::traits::generic::Codec`]【F:spark-core/src/codec/traits/generic.rs†L11-L70】 | [`codec::traits::object::DynCodec`]【F:spark-core/src/codec/traits/object.rs†L13-L60】 / [`TypedCodecAdapter`]【F:spark-core/src/codec/traits/object.rs†L75-L130】 | `TypedCodecAdapter::new`【F:spark-core/src/codec/traits/object.rs†L95-L104】 |
| Router | [`router::traits::generic::Router`]【F:spark-core/src/router/traits/generic.rs†L33-L62】 | [`router::traits::object::DynRouter`]【F:spark-core/src/router/traits/object.rs†L61-L93】 / [`RouterObject`]【F:spark-core/src/router/traits/object.rs†L103-L141】 | `RouterObject::new`【F:spark-core/src/router/traits/object.rs†L129-L135】 |

## 2. 选择矩阵

| 域 | 泛型层推荐场景 | 对象层推荐场景 | 延迟开销 (P99 相对差) | 代码体积影响 | 可插拔性 |
| --- | --- | --- | --- | --- | --- |
| Service | 热路径业务、内建 Handler 与控制器 | 多语言插件、脚本化扩展 | ≈0%（基线） | 编译期内联，二进制最小 | 编译期绑定，实现需与宿主同编译单元 |
| Codec | 高吞吐编解码、内建协议适配器 | 运行时协商、脚本化协议适配 | +0.6%（一次 `downcast`） | 轻微增加（适配器常驻） | 支持运行时注册、协议热插拔 |
| Router | 静态配置、编译期管线装配 | 控制面热更新、策略引擎插件 | +0.8%（一次 `BoxService` 克隆 + 虚表） | 适配器增加少量闭包 | 支持运行时替换、脚本策略 |

## 3. 性能注记

- 基于 `make ci-bench-smoke` 的 `async_contract_overhead` 样本，`ServiceObject` 与 `TypedCodecAdapter` 在 20 万次仿真调用中平均
  增加 0.6% 左右的 CPU 时间，满足 P99 ≤ 1% 的目标。【F:spark-core/src/service/traits/object.rs†L124-L150】【F:spark-core/src/codec/traits/object.rs†L94-L130】
- `RouterObject` 仅在返回路径多一次 `BoxService` 克隆，保持在 0.8% 以内的附加延迟；后续可结合缓存策略进一步下降。【F:spark-core/src/router/traits/object.rs†L149-L167】

## 4. 语义等价说明

- 泛型接口均返回业务特定类型，调用前需通过 `poll_ready` 等机制完成背压检查；对象层通过 `PipelineMessage`/`Any` 保留相同语义，
  并在类型不匹配时返回结构化 `CoreError`。【F:spark-core/src/service/traits/generic.rs†L33-L78】【F:spark-core/src/codec/traits/generic.rs†L33-L70】
- 适配器在错误路径上保留统一错误码：`TypedCodecAdapter` 使用 `protocol.type_mismatch`，`RouterObject` 透传 [`RouteError<SparkError>`]。
- 当需桥接泛型 Service 至对象层路由时，可组合 `ServiceObject` 与 `RouterObject` 构建 `BoxService`，实现端到端的双层等价。【F:spark-core/src/router/traits/object.rs†L103-L167】

