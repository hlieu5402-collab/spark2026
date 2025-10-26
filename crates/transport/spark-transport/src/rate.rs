use alloc::boxed::Box;
use core::time::Duration;

/// 速率限制器在传输层的统一契约。
///
/// # 教案级注释
///
/// ## 意图（Why）
/// - 为连接/监听器提供统一的速率许可申请接口，支持基于令牌桶、漏桶或自适应算法的实现。
/// - 允许在替换传输协议时沿用同一限流策略，避免在业务层重复接线。
///
/// ## 契约说明（What）
/// - `try_acquire`：尝试申请 `demand` 个许可，立即返回 [`RatePermit`] 或错误；
/// - `RatePermit`：在 `Drop` 时归还许可，可选择携带推荐的下一次申请时间。
/// - **前置条件**：`demand > 0` 且调用方尊重返回的退避建议；
/// - **后置条件**：成功取得的许可必须在使用完后释放，以避免饥饿。
///
/// ## 风险提示（Trade-offs）
/// - 回调基于 `Box<dyn FnOnce()>`，若实现追求零分配，可自定义 Permit 类型。
/// - `recommended_retry_after` 仅为建议，调用方可结合自身策略做二次决策。
pub trait RateLimiter: Send + Sync {
    /// 申请速率许可。
    fn try_acquire<'a>(&'a self, demand: u32) -> Result<RatePermit<'a>, &'static str>;
}

/// 速率许可的生命周期守卫。
pub struct RatePermit<'a> {
    release: Option<Box<dyn FnOnce() + Send + 'a>>,
    recommended_retry_after: Option<Duration>,
}

impl<'a> RatePermit<'a> {
    /// 构造新的许可。
    pub fn new<F>(release: F, retry_after: Option<Duration>) -> Self
    where
        F: FnOnce() + Send + 'a,
    {
        Self {
            release: Some(Box::new(release)),
            recommended_retry_after: retry_after,
        }
    }

    /// 返回推荐的下一次申请等待时间。
    pub fn recommended_retry_after(&self) -> Option<Duration> {
        self.recommended_retry_after
    }
}

impl Drop for RatePermit<'_> {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}
