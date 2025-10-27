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
