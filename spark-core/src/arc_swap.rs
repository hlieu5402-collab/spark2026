//! 条件性 `ArcSwap` 适配层。
//!
//! # 设计初衷（Why）
//! - 在 `std` 特性可用时复用社区成熟的 [`arc-swap`](https://crates.io/crates/arc-swap) 实现，保证锁自由的读路径。
//! - 在 `no_std + alloc` 场景下，该三方库需要 nightly 才能启用实验特性，不符合本仓库 Rust 1.89 稳定版的基线。
//! - 因此提供一个轻量级回退实现，以相同接口保障编译通过，并在后续具备稳定支持时无缝切换到官方版本。
//!
//! # 使用方式（How）
//! - 业务代码统一通过 `crate::arc_swap::ArcSwap` 导入类型。
//! - 对于 `std` 构建，本模块直接 `pub use` 第三方实现；对于纯 `alloc` 构建，改为使用内部 `spin::RwLock` 封装的仿制结构。
//!
//! # 契约说明（What）
//! - API 保持与 `arc-swap` 最常用的四个方法兼容：`new`、`from_pointee`、`load_full`、`store`。
//! - 回退实现保证线程安全与 `Arc` 快照语义，但不提供锁自由特性；调用方仍可在功能层面正常运行。
//!
//! # 权衡与注意事项（Trade-offs）
//! - 回退实现使用自旋锁保持 `no_std` 可用性，会牺牲部分性能；但 `alloc` 构建通常用于受限环境，允许以正确性优先。
//! - 一旦上游库提供稳定的 `no_std` 支持，可移除回退实现，恢复纯粹依赖第三方 crate。

#[cfg(feature = "std")]
pub use ::arc_swap::ArcSwap;

#[cfg(not(feature = "std"))]
mod fallback {
    use alloc::sync::Arc;
    use core::fmt;
    use spin::RwLock;

    /// `no_std` 环境下的精简 `ArcSwap` 仿制实现。
    ///
    /// - **意图（Why）**：在未启用 `std` 时维持与上层契约兼容的 API，避免大量条件编译分支。
    /// - **逻辑（How）**：内部使用 `spin::RwLock<Arc<T>>` 保存快照；读操作获取共享锁并克隆 `Arc`；写操作获取独占锁并替换。
    /// - **契约（What）**：确保读操作返回的 `Arc<T>` 与写入值一致且具备引用计数语义；需要调用方保证 `T: Send + Sync` 以跨线程共享。
    /// - **注意事项（Trade-offs）**：该版本为阻塞实现，写操作会短暂阻塞读者；在极端低延迟场景应谨慎使用。
    pub struct ArcSwap<T> {
        inner: RwLock<Arc<T>>,
    }

    impl<T> ArcSwap<T> {
        /// 构造新的交换容器。
        ///
        /// - **输入参数**：`initial` 为初始快照，需已封装在 `Arc` 中。
        /// - **前置条件**：调用方需保证传入的 `Arc<T>` 在构造后不再被可变借用。
        /// - **后置条件**：容器持有 `initial` 的克隆，后续 `load_full` 将返回等价快照。
        pub fn new(initial: Arc<T>) -> Self {
            Self {
                inner: RwLock::new(initial),
            }
        }

        /// 以值语义构造容器，内部自动封装为 `Arc`。
        ///
        /// - **输入参数**：`value` 为所有权转移的初始内容。
        /// - **实现逻辑**：直接调用 [`ArcSwap::new`]，减少重复代码。
        pub fn from_pointee(value: T) -> Self {
            Self::new(Arc::new(value))
        }

        /// 读取当前快照。
        ///
        /// - **实现逻辑**：获取读锁后克隆内部 `Arc`，以零拷贝方式共享底层数据。
        /// - **注意事项**：克隆操作仅增加引用计数，不会复制 `T`。
        pub fn load_full(&self) -> Arc<T> {
            self.inner.read().clone()
        }

        /// 用新的快照替换当前值。
        ///
        /// - **输入参数**：`value` 为新的共享状态。
        /// - **实现逻辑**：获取写锁并覆盖原有 `Arc`；旧快照在所有持有者释放后自动回收。
        /// - **并发语义**：写操作会短暂阻塞其他读写方，符合自旋锁的公平性假设。
        pub fn store(&self, value: Arc<T>) {
            *self.inner.write() = value;
        }
    }

    impl<T: fmt::Debug> fmt::Debug for ArcSwap<T> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("ArcSwap")
                .field("inner", &self.inner.read())
                .finish()
        }
    }
}

#[cfg(not(feature = "std"))]
pub use fallback::ArcSwap;
