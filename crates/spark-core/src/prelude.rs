#![allow(clippy::module_name_repetitions)]

//! # spark-core Prelude
//!
//! ## 教案级说明（Why）
//! - **统一导入面**：为上层 crate 提供一个稳定、浅路径的导入入口，
//!   避免在业务代码中出现大量 `spark_core::contract::...` 等深层次路径，
//!   从而降低“复制粘贴 + 临时重定义”带来的契约分叉风险。
//! - **体系定位**：该模块位于 `spark-core` 的最外层，面向使用者暴露“常用契约组合包”，
//!   是 `pipeline`、`buffers`、`transport` 等上层组件构建的必经入口。
//! - **设计思路**：遵循 Rust 社区常见的 Prelude 模式，通过精选的 re-export 集合，
//!   让依赖方仅需 `use spark_core::prelude::*;` 即可获取常用类型。
//!
//! ## 逻辑拆解（How）
//! 1. **常量/别名 re-export**：直接透出核心错误类型与 `Result` 别名，
//!    保持错误语义统一；
//! 2. **契约类型聚合**：集中导出调用上下文（`CallContext` 等）、预算（`Budget*` 系列）、
//!    管道扩展（`PipelineMessage` 等）以及运行时能力（`ExecutionContext`、`CoreServices` 等）；
//! 3. **状态机语义**：包含 `status::ready` 中的核心判定枚举，支撑背压与管道调度；
//! 4. **传输层契约**：将 `TransportSocketAddr`、`ShutdownDirection` 等常用结构一并暴露，
//!    便于传输实现 crate 直接复用；
//! 5. **异步工具**：预置 `BoxFuture`、`BoxStream`，方便在无 `std` 环境下进行动态分发。
//!
//! ## 契约定义（What）
//! - **输入前置**：调用方应基于 `spark-core` 公布的稳定接口使用本 Prelude，
//!   不应假设其包含试验性或内部模块；
//! - **输出保证**：成功导入后，可稳定访问下列 re-export 的类型与函数；
//! - **版本策略**：Prelude 仅收录稳定契约；新增导出将遵循 SemVer，可向后兼容。
//!
//! ## 设计考量与权衡（Trade-offs）
//! - **范围控制**：为防止 Prelude 无限膨胀，仅纳入“跨模块高频依赖”的类型。
//!   对于边缘模块（例如观测、审计）仍建议使用明确命名空间以提升可读性；
//! - **性能影响**：纯 re-export 不引入额外代码路径，对编译与运行时零开销；
//! - **维护成本**：若核心契约迁移，需要同步更新此处映射，
//!   因此在代码评审中需重点关注 Prelude 的变更是否与稳定性策略一致。

/// ## 导出明细（How）
/// - **异步与工具**：`async_trait`、`BoxFuture`、`BoxStream` 支撑多态异步实现；
/// - **上下文契约**：`ExecutionContext`、`CallContext`、`Cancellation`、`Deadline` 等
///   描述一次调用的生命周期与预算约束；
/// - **管道缓冲**：`PipelineMessage`、`ReadableBuffer`、`WritableBuffer` 等保证消息在各阶段流转；
/// - **错误语义**：统一暴露 `CoreError`、`SparkError`、`Result` 以及错误分类枚举；
/// - **状态与背压**：`ReadyState`、`RetryAdvice` 等用于实现传输与管道的背压策略；
/// - **运行时契约**：`CoreServices`、`MonotonicTimePoint` 帮助在无 `std` 环境下调度任务；
/// - **传输抽象**：`TransportSocketAddr`、`ShutdownDirection` 让传输实现保持一致行为。
pub use crate::{
    async_trait,
    buffer::{
        BufView, BufferAllocator, BufferPool, Bytes, PipelineMessage, PoolStats, ReadableBuffer,
        WritableBuffer,
    },
    context::ExecutionContext,
    contract::{
        Budget, BudgetDecision, BudgetKind, BudgetSnapshot, CallContext, CallContextBuilder,
        Cancellation, CloseReason, DEFAULT_OBSERVABILITY_CONTRACT, Deadline,
    },
    error::{
        CoreError, DomainError, DomainErrorKind, ErrorCategory, ErrorCause, ImplError,
        ImplErrorKind, IntoCoreError, IntoDomainError, Result, SparkError,
    },
    future::{BoxFuture, BoxStream, Stream},
    observability::{CoreUserEvent, LogRecord, Logger, TraceContext},
    runtime::{CoreServices, JoinHandle, MonotonicTimePoint},
    service::{
        AutoDynBridge, BoxService, Decode, DynService, Encode, Layer, Service, ServiceObject,
        type_mismatch_error,
    },
    status::ready::{BusyReason, PollReady, ReadyCheck, ReadyState, RetryAdvice},
    transport::{ShutdownDirection, TransportSocketAddr},
};
//! `spark_core::prelude`：契约级常用类型一站式导入。
//!
//! # 设计意图（Why）
//! - 降低新接入方的学习门槛，仅需 `use spark_core::prelude::*;` 即可获取三元组、错误、ID 等核心概念；
//! - 避免业务侧错误导出内部模块（例如 `configuration`），确保依赖面受控；
//! - 支持 `no_std + alloc` 环境，全部类型均来源于本 crate。
//!
//! # 收录内容（What）
//! - 调用三元组：[`CallContext`]、[`Cancellation`]、[`Deadline`];
//! - 错误体系：[`CoreError`]、[`Result`];
//! - 预算与协议：[`Budget`]、[`BudgetDecision`]、[`Event`]、[`Frame`]、[`Message`];
//! - 标识与配置：[`RequestId`]、[`CorrelationId`]、[`IdempotencyKey`]、[`Timeout`];
//! - 状态语义：[`State`]、[`Status`];
//! - 辅助类型：[`NonEmptyStr`]、[`CloseReason`], [`BudgetSet`], [`TimeoutProfile`].

pub use crate::{
    CallContext, CallContextBuilder, Cancellation, CloseReason, CoreError, Deadline, Event, Frame,
    IdempotencyKey, Message, NonEmptyStr, RequestId, Result, State, Status, Timeout,
    TimeoutProfile,
};

pub use crate::ids::CorrelationId;
pub use crate::types::{Budget, BudgetDecision, BudgetKind, BudgetSet, BudgetSnapshot};
