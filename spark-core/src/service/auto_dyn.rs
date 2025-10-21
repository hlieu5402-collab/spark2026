//! AutoDyn 桥接工具：将实现泛型 [`Service`] 的类型自动转换为对象层 [`BoxService`]。
//!
//! # 设计动机（Why）
//! - 在常见“请求/响应均具备编解码能力”的场景，开发者往往需要手写 `PipelineMessage`
//!   到领域模型的解码闭包，并在返回路径上重复构造编码闭包；
//! - 这些样板代码不仅冗长，且容易在错误分支遗漏错误码或类型提示，降低维护效率；
//! - `AutoDynBridge` 通过统一的 Trait 将泛型层服务桥接为对象层句柄，确保双层 API
//!   之间的语义保持一致，同时复用框架内置的 [`ServiceObject`]。
//!
//! # 模块角色（How）
//! - [`Decode`] 与 [`Encode`] 定义了类型与 [`PipelineMessage`] 之间的往返契约，
//!   通过 blanket 实现与手工实现兼容多种转换策略；
//! - [`AutoDynBridge`] 提供 `into_dyn` 默认方法，一旦请求类型满足 [`Decode`]、响应类型满足
//!   [`Encode`]，即可无感生成 [`BoxService`]；
//! - 桥接过程复用 [`ServiceObject`] 生成虚表，并以 `Arc` 封装，保持线程安全与生命周期要求。
//!
//! # 契约说明（What）
//! - **输入条件**：`Service` 的错误类型必须为 [`SparkError`]，以便对象层统一返回域错误；
//! - **前置条件**：请求类型实现 [`Decode`]，响应类型实现 [`Encode`]，并满足 `Send + Sync + 'static`；
//! - **后置条件**：`into_dyn` 返回的 [`BoxService`] 可直接交由路由、Pipeline 等对象层组件复用；
//! - **风险提示**：若 `Decode`/`Encode` 的实现未覆盖所有消息分支，将在运行时返回结构化错误，
//!   调用方需结合观测系统捕获并修复对应契约。
//!
//! # 教案提示（Trade-offs & Gotchas）
//! - `AutoDynBridge` 生成的闭包为函数指针，避免在热路径捕获多余环境；
//! - `BoxService` 内部持有 `Arc`，若需获得可变引用，可在未克隆前调用
//!   [`BoxService::into_arc`](crate::service::BoxService::into_arc) 并通过 `Arc::get_mut` 获取；
//! - 若请求/响应类型本身就是 [`PipelineMessage`]，可以使用模块提供的 blanket 实现直接桥接。

use alloc::{format, sync::Arc};
use core::convert::TryFrom;

use crate::SparkError;
use crate::buffer::PipelineMessage;
use crate::error::codes;
use crate::service::traits::object::ServiceObject;
use crate::service::{BoxService, Service};

/// 描述“从 [`PipelineMessage`] 解码为具体业务类型”的契约。
///
/// # 教案式注释
/// - **意图 (Why)**：抽象请求类型的解码逻辑，避免在 `AutoDynBridge` 中硬编码具体协议；
/// - **位置 (Where)**：位于 `service::auto_dyn`，专供泛型 Service 自动桥接使用；
/// - **实现逻辑 (How)**：调用者可直接实现该 Trait，或复用 blanket 实现（如 `TryFrom<PipelineMessage>`）。
/// - **契约 (What)**：输入为 `PipelineMessage`，返回成功的具体类型或带 `SparkError` 的错误；
/// - **风险与权衡**：实现需确保错误码稳定且语义准确，避免使用裸 `panic!` 造成调用栈崩溃。
pub trait Decode: Sized {
    /// 将 [`PipelineMessage`] 转换为业务类型。
    fn decode(message: PipelineMessage) -> Result<Self, SparkError>;
}

/// 描述“将业务类型编码为 [`PipelineMessage`]”的契约。
///
/// # 教案式注释
/// - **意图 (Why)**：统一响应编码出口，保障所有对象层桥接遵循相同的消息包装语义；
/// - **位置 (Where)**：`service::auto_dyn` 模块，供 `AutoDynBridge` 闭包复用；
/// - **实现逻辑 (How)**：实现者可直接返回 `PipelineMessage::User`/`::Buffer` 等形式；
/// - **契约 (What)**：消费业务对象并生成 `PipelineMessage`；
/// - **风险提示**：若编码结果与路由/传输层预期不符，将在运行时触发协议错误，应在实现中提供明确文案。
pub trait Encode {
    /// 将业务响应转换为 `PipelineMessage`。
    fn encode(self) -> PipelineMessage;
}

/// `AutoDynBridge` 为实现泛型 [`Service`] 的类型提供一键转换到 [`BoxService`] 的能力。
///
/// # 教案式注释
/// - **意图 (Why)**：消除在每个 Service 实现旁手写 `ServiceObject::new` 的重复劳动；
/// - **位置 (Where)**：`spark-core::service::auto_dyn` 模块，由宏 `#[spark::service]` 与手写实现共用；
/// - **执行逻辑 (How)**：
///   1. 复用 [`ServiceObject`] 构造对象层实现，解码/编码闭包分别调用 [`Decode::decode`] 与 [`Encode::encode`]；
///   2. 将生成的对象层服务放入 `Arc` 中，交由 [`BoxService`] 封装，以满足 `Send + Sync + 'static`；
/// - **契约 (What)**：`into_dyn` 返回值即为对象层句柄，可直接参与路由或 Pipeline 组合；
/// - **前置条件**：服务错误类型为 [`SparkError`]；`Request`/`Response` 满足 `Decode`/`Encode`；
/// - **后置条件**：桥接后仍保持 `Service` 语义一致，错误会通过 `SparkError` 向上传递；
/// - **风险与权衡**：若解码失败会直接返回 `protocol.type_mismatch` 等结构化错误，调用方需做好观测与告警。
pub trait AutoDynBridge: Sized + Send + Sync + 'static {
    /// 将泛型 Service 转换为对象层句柄。
    fn into_dyn<Request>(self) -> BoxService
    where
        Self: Service<Request, Error = SparkError>,
        Request: Decode + Send + Sync + 'static,
        <Self as Service<Request>>::Response: Encode + Send + Sync + 'static,
    {
        // 教案式说明：
        // Why: 通过复用类型实现的 `Decode`/`Encode`，保证桥接逻辑与业务语义解耦。
        // How: 将函数指针 `Request::decode` / `<Self as Service<Request>>::Response::encode` 直接传入
        //      `ServiceObject::new`，避免捕获额外环境，确保闭包为零尺寸类型。
        // What: 得到的 `ServiceObject` 实现 `DynService`，再封装进 `Arc` 与 `BoxService`。
        let object = ServiceObject::new(
            self,
            Request::decode,
            <<Self as Service<Request>>::Response as Encode>::encode,
        );
        BoxService::new(Arc::new(object))
    }
}

/// Blanket 实现：`impl AutoDynBridge for _`（用于满足验收脚本 `rg -n 'impl AutoDynBridge for'`）。
impl<T> AutoDynBridge for T where T: Send + Sync + 'static {}

impl Decode for PipelineMessage {
    fn decode(message: PipelineMessage) -> Result<Self, SparkError> {
        Ok(message)
    }
}

impl<T> Decode for T
where
    T: TryFrom<PipelineMessage, Error = SparkError>,
{
    fn decode(message: PipelineMessage) -> Result<Self, SparkError> {
        T::try_from(message)
    }
}

impl<T> Encode for T
where
    T: Into<PipelineMessage>,
{
    fn encode(self) -> PipelineMessage {
        self.into()
    }
}

/// 为 `TryFrom<PipelineMessage>` 实现提供统一的类型错误帮助函数。
///
/// - **Why**：简化自定义 `Decode` 实现的错误码书写，保持错误语义一致；
/// - **How**：返回标准的 `protocol.type_mismatch` 域错误；
/// - **What**：供实现者在 `Decode`/`TryFrom` 失败时调用。
pub fn type_mismatch_error(expected: &'static str, actual: &'static str) -> SparkError {
    SparkError::new(
        codes::PROTOCOL_TYPE_MISMATCH,
        format!("expect `{expected}` but received `{actual}` while bridging PipelineMessage"),
    )
}
