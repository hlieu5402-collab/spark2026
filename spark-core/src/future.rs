use alloc::boxed::Box;
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

/// `BoxFuture` 是 `spark-core` 在 `no_std + alloc` 下使用的通用 Future 包装。
///
/// # 设计背景（Why）
/// - 统一 Future 表达，避免依赖外部 crate。
///
/// # 契约说明（What）
/// - 约束 Future 为 `Send + 'a`，可安全跨线程。
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// `LocalBoxFuture` 封装 `!Send` Future。
///
/// # 设计背景（Why）
/// - 为了支撑单线程执行器（如浏览器 WebAssembly、嵌入式事件循环），需要一个不要求 `Send`
///   的通用 Future 包装。
///
/// # 契约说明（What）
/// - 仅需满足 `'a` 生命周期约束，允许运行在调用方指定的线程或任务上下文。
pub type LocalBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// `Stream` 描述按需拉取元素的异步序列。
///
/// # 设计背景（Why）
/// - 兼容 `futures_core::Stream` 接口，确保契约一致。
///
/// # 契约说明（What）
/// - `poll_next` 与标准 Stream 语义一致，返回 `Poll<Option<Item>>`。
pub trait Stream {
    /// 流中产生的元素类型。
    type Item;

    /// 从流中轮询下一个元素。
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>>;
}

/// `BoxStream` 为对象安全的流包装。
///
/// # 契约说明（What）
/// - 封装任何实现 `Stream + Send` 的类型，生命周期由 `'a` 限定。
pub type BoxStream<'a, T> = Pin<Box<dyn Stream<Item = T> + Send + 'a>>;
