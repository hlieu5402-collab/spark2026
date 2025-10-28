use alloc::{borrow::Cow, boxed::Box, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use bytes::BytesMut;
use spin::Mutex;

use spark_core::{
    CoreError, Result,
    buffer::{
        BufferPool, ErasedSparkBuf, ErasedSparkBufMut, PoolStatDimension, PoolStats, WritableBuffer,
    },
};

use crate::pooled_buffer::{BufferRecycler, PooledBuffer, ReclaimedBuffer};

/// `SlabBufferPool` 提供基于自由链表（Free List）的缓冲池实现，
/// 专注在**高并发、低延迟**场景下复用 `BytesMut`，以减少堆分配次数。
///
/// # 模块角色（Why）
/// - 作为 `spark-core::buffer::BufferPool` 的默认实现，为运行时、协议栈提供统一的缓冲来源；
/// - 借助 `PooledBuffer` 的生命周期钩子，在 `Drop` 阶段自动回收 `BytesMut`，避免调用方关注回收细节；
/// - 对外暴露 `alloc_readable`/`alloc_writable` 工厂方法，便于直接生成对象安全的缓冲引用。
///
/// # 核心机制（How）
/// - 内部维护 `spin::Mutex<Vec<BytesMut>>` 作为自由链表，租借时优先复用足够大的块，减少重新分配；
/// - `PoolMetrics` 通过原子计数跟踪 `allocated_bytes`、`available_bytes`、`active_leases` 等指标，
///   支撑 `statistics` 快照以及后续的监控集成；
/// - `BufferRecycler` 实现中使用 `ReclaimedBuffer` 获取回收上下文，既更新统计也将 `BytesMut` 放回链表。
///
/// # 契约说明（What）
/// - **线程安全**：所有共享状态均通过 `spin::Mutex` 与原子计数保护，满足 `Send + Sync + 'static` 约束；
/// - **前置条件**：调用方需保证 `min_capacity` 表示真实需求；若为 0，将返回最小容量的可写缓冲；
/// - **后置条件**：`alloc_writable` 返回的缓冲满足 `remaining_mut() >= min_capacity`，
///   `alloc_readable` 会将输入字节完整写入并冻结为只读视图。
///
/// # 设计权衡（Trade-offs）
/// - 使用自旋锁（`spin::Mutex`）而非 `parking_lot::Mutex`，以便在 `no_std`/线程数量有限的环境中仍能工作；
/// - 回收失败（无法重新获得 `BytesMut`）时，仅更新统计并在下次租借时重新分配，
///   牺牲部分性能换取语义稳定性；
/// - `shrink_to_fit` 采取“清空自由链表”的简单策略，便于在压测后快速归还峰值内存。
#[derive(Clone)]
pub struct SlabBufferPool {
    inner: Arc<PoolInner>,
}

impl Default for SlabBufferPool {
    fn default() -> Self {
        Self {
            inner: Arc::new(PoolInner::new()),
        }
    }
}

impl SlabBufferPool {
    /// 创建空池实例，供运行时注入或测试场景直接使用。
    pub fn new() -> Self {
        Self::default()
    }

    /// 分配并填充一个只读缓冲。
    ///
    /// # 参数与契约
    /// - `data`：待写入的原始字节切片，允许为空；
    /// - **前置条件**：调用方无需持有其它租借；方法内部会根据长度计算最小容量；
    /// - **后置条件**：返回的 `ErasedSparkBuf` 中 `remaining()` 等于 `data.len()`，
    ///   且可被安全地拆分、拷贝。
    ///
    /// # 实现策略
    /// 1. 复用 `alloc_writable` 的核心逻辑，确保统计与回收路径一致；
    /// 2. 将输入数据写入 `PooledBuffer` 后立即 `freeze`，生成只读视图；
    /// 3. 若 `put_slice` 过程中触发扩容，将自动刷新租约容量，保证回收时统计正确。
    pub fn alloc_readable(&self, data: &[u8]) -> Result<Box<ErasedSparkBuf>, CoreError> {
        let mut writable: Box<PooledBuffer> = Box::new(self.allocate_pooled(data.len())?);
        if !data.is_empty() {
            writable.put_slice(data)?;
        }
        writable.freeze()
    }

    /// 分配一个可写缓冲，满足最小容量约束。
    ///
    /// # 参数与契约
    /// - `min_capacity`：调用方期望的最小可写空间；
    /// - **后置条件**：返回对象满足 `WritableBuffer` 契约，容量至少等于 `min_capacity`；
    /// - **异常处理**：当前实现不会返回错误，但保留 `Result` 以对齐 trait 约束。
    pub fn alloc_writable(&self, min_capacity: usize) -> Result<Box<ErasedSparkBufMut>, CoreError> {
        let buffer: Box<PooledBuffer> = Box::new(self.allocate_pooled(min_capacity)?);
        Ok(buffer)
    }

    /// 实际的缓冲构建逻辑，供公开 API 与 trait 方法复用。
    fn allocate_pooled(&self, min_capacity: usize) -> Result<PooledBuffer, CoreError> {
        let raw = self.inner.acquire_buffer(min_capacity);
        let recycler: Arc<dyn BufferRecycler> = self.inner.clone();
        Ok(PooledBuffer::new(raw, recycler))
    }
}

impl BufferPool for SlabBufferPool {
    fn acquire(&self, min_capacity: usize) -> Result<Box<dyn WritableBuffer>, CoreError> {
        let buffer: Box<PooledBuffer> = Box::new(self.allocate_pooled(min_capacity)?);
        Ok(buffer)
    }

    fn shrink_to_fit(&self) -> Result<usize, CoreError> {
        Ok(self.inner.shrink_free_list())
    }

    fn statistics(&self) -> Result<PoolStats, CoreError> {
        Ok(self.inner.snapshot())
    }
}

struct PoolInner {
    free_list: Mutex<Vec<BytesMut>>,
    metrics: PoolMetrics,
}

impl PoolInner {
    fn new() -> Self {
        Self {
            free_list: Mutex::new(Vec::new()),
            metrics: PoolMetrics::default(),
        }
    }

    /// 从自由链表或堆上获取一个满足容量的 `BytesMut`。
    fn acquire_buffer(&self, min_capacity: usize) -> BytesMut {
        let reused = {
            let mut list = self.free_list.lock();
            if let Some(index) = list.iter().position(|buf| buf.capacity() >= min_capacity) {
                let mut buf = list.swap_remove(index);
                let capacity = buf.capacity();
                buf.clear();
                self.metrics.decrease_available(capacity);
                Some(buf)
            } else {
                None
            }
        };

        let mut buffer = match reused {
            Some(buf) => buf,
            None => {
                let buf = BytesMut::with_capacity(min_capacity);
                let capacity = buf.capacity();
                self.metrics.increase_on_new_allocation(capacity);
                buf
            }
        };
        buffer.clear();
        self.metrics.increase_active_leases();
        buffer
    }

    fn shrink_free_list(&self) -> usize {
        let mut list = self.free_list.lock();
        let reclaimed: usize = list.iter().map(BytesMut::capacity).sum();
        list.clear();
        self.metrics.decrease_on_shrink(reclaimed);
        reclaimed
    }

    fn snapshot(&self) -> PoolStats {
        let free_slots = self.free_list.lock().len();
        PoolStats {
            allocated_bytes: self.metrics.allocated_bytes.load(Ordering::Relaxed),
            resident_bytes: self.metrics.resident_bytes.load(Ordering::Relaxed),
            active_leases: self.metrics.active_leases.load(Ordering::Relaxed),
            available_bytes: self.metrics.available_bytes.load(Ordering::Relaxed),
            pending_lease_requests: 0,
            failed_acquisitions: self.metrics.failed_acquisitions.load(Ordering::Relaxed),
            custom_dimensions: vec![PoolStatDimension {
                key: Cow::Borrowed("slab_free_slots"),
                value: free_slots,
            }],
        }
    }
}

impl BufferRecycler for PoolInner {
    fn reclaim(&self, reclaimed: ReclaimedBuffer) {
        self.metrics.decrease_active_leases();
        let capacity = reclaimed.capacity();
        match reclaimed.into_buffer() {
            Some(mut buf) => {
                buf.clear();
                self.metrics.increase_available(capacity);
                self.free_list.lock().push(buf);
            }
            None => {
                self.metrics.decrease_on_loss(capacity);
            }
        }
    }
}

#[derive(Default)]
struct PoolMetrics {
    allocated_bytes: AtomicUsize,
    resident_bytes: AtomicUsize,
    available_bytes: AtomicUsize,
    active_leases: AtomicUsize,
    failed_acquisitions: AtomicU64,
}

impl PoolMetrics {
    fn increase_on_new_allocation(&self, capacity: usize) {
        self.allocated_bytes.fetch_add(capacity, Ordering::Relaxed);
        self.resident_bytes.fetch_add(capacity, Ordering::Relaxed);
    }

    fn increase_available(&self, capacity: usize) {
        self.available_bytes.fetch_add(capacity, Ordering::Relaxed);
    }

    fn decrease_available(&self, capacity: usize) {
        saturating_sub(&self.available_bytes, capacity);
    }

    fn decrease_on_loss(&self, capacity: usize) {
        saturating_sub(&self.allocated_bytes, capacity);
        saturating_sub(&self.resident_bytes, capacity);
    }

    fn decrease_on_shrink(&self, capacity: usize) {
        self.decrease_available(capacity);
        self.decrease_on_loss(capacity);
    }

    fn increase_active_leases(&self) {
        self.active_leases.fetch_add(1, Ordering::Relaxed);
    }

    fn decrease_active_leases(&self) {
        let _ = self
            .active_leases
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |prev| {
                Some(prev.saturating_sub(1))
            });
    }
}

fn saturating_sub(target: &AtomicUsize, value: usize) {
    let _ = target.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_sub(value))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reusable_capacity_returns_to_pool() {
        let pool = SlabBufferPool::new();
        {
            let mut writable = pool.alloc_writable(64).expect("租借缓冲失败");
            assert!(writable.remaining_mut() >= 64);
            writable.put_slice(&[1, 2, 3, 4]).expect("写入测试数据");
        }
        let snapshot = pool.statistics().expect("读取统计失败");
        assert!(snapshot.available_bytes >= 64);
        {
            let _second = pool.alloc_writable(16).expect("复用缓冲失败");
        }
        let after = pool.statistics().expect("读取统计失败");
        assert!(after.allocated_bytes >= snapshot.allocated_bytes);
    }

    #[test]
    fn alloc_readable_preserves_payload() {
        let pool = SlabBufferPool::new();
        let payload = [9u8, 8, 7, 6];
        let mut readable = pool.alloc_readable(&payload).expect("分配只读缓冲失败");
        let mut out = [0u8; 4];
        readable.copy_into_slice(&mut out).expect("读取数据失败");
        assert_eq!(out, payload);
    }
}
