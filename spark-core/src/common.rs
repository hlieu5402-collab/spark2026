use core::any::Any;

use crate::sealed::Sealed;

/// 表示空返回值，常用于无需携带数据的响应。
///
/// # 设计背景（Why）
/// - 某些 Service/Handler 仅需表示成功，无额外载荷。
///
/// # 契约说明（What）
/// - `Empty` 可作为泛型响应类型的占位符，序列化层可选择忽略或输出固定内容。
#[derive(Debug, Clone, Copy)]
pub struct Empty;

/// 将某个类型转换为空值。
///
/// # 契约说明（What）
/// - 提供统一的 `into_empty` 方法，便于在泛型上下文中将 `()` 等类型转为 `Empty`。
pub trait IntoEmpty: Sealed {
    /// 消费自身并返回 `Empty`。
    fn into_empty(self) -> Empty;
}

impl IntoEmpty for () {
    fn into_empty(self) -> Empty {
        Empty
    }
}

/// Loopback 提供在 Handler 内部向自身注入事件的能力。
///
/// # 设计背景（Why）
/// - 支持 Handler 在异步任务完成后回调自身，例如触发补发或心跳。
///
/// # 契约说明（What）
/// - `fire_loopback_inbound` 将事件注入入站链路，`fire_loopback_outbound` 注入出站链路。
/// - 事件类型要求实现 `Any + Send + Sync`，以保证跨线程安全。
///
/// # 风险提示（Trade-offs）
/// - 循环注入可能导致无限递归，调用方需自行控制次数或添加去抖逻辑。
pub trait Loopback: Send + Sync + Sealed {
    /// 向入站路径注入事件。
    fn fire_loopback_inbound(&self, event: impl Any + Send + Sync);

    /// 向出站路径注入事件。
    fn fire_loopback_outbound(&self, event: impl Any + Send + Sync);
}
