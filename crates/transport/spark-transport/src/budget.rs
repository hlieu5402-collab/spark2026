use alloc::boxed::Box;

/// 表示可被消耗的执行预算（例如重试次数、最大并发）。
///
/// # 教案级注释
///
/// ## 意图（Why）
/// - 在传输实现与上层协议之间统一“预算”语义，避免 TCP/QUIC/TLS 分别使用不同的计数器类型。
/// - 允许实现将预算来源（如调用方上下文、集群限流器）封装为闭包，在 guard 释放时归还配额。
///
/// ## 契约说明（What）
/// - `try_acquire`：尝试消耗给定数量的预算，成功后返回 [`BudgetGuard`]；失败时返回错误。
/// - **前置条件**：`amount > 0`，调用方确保预算类型支持该度量（次数/字节等）。
/// - **后置条件**：成功返回的 guard 在 `Drop` 时触发归还逻辑。
///
/// ## 风险提示（Trade-offs）
/// - Guard 采用 `Box<dyn FnOnce()>` 存储回调，便于在 `no_std + alloc` 环境下运行；
/// - 如果实现需要零分配，可自定义结构体实现该 trait 并在 `BudgetGuard::new` 中复用静态闭包。
pub trait Budget: Send + Sync {
    /// 消耗指定数量的预算。
    fn try_acquire<'a>(&'a self, amount: u32) -> crate::Result<BudgetGuard<'a>, &'static str>;
}

/// 预算占用的生命周期守卫，负责在 `Drop` 时归还配额。
pub struct BudgetGuard<'a> {
    release: Option<Box<dyn FnOnce() + Send + 'a>>,
}

impl<'a> BudgetGuard<'a> {
    /// 构造一个新的 Guard。
    pub fn new<F>(release: F) -> Self
    where
        F: FnOnce() + Send + 'a,
    {
        Self {
            release: Some(Box::new(release)),
        }
    }
}

impl Drop for BudgetGuard<'_> {
    fn drop(&mut self) {
        if let Some(callback) = self.release.take() {
            callback();
        }
    }
}
