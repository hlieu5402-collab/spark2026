use spark_core::{BoxFuture, BoxStream, Stream};
use std::env;
use std::hint::black_box;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::time::{Duration, Instant};

/// 基准测试入口：比较 `BoxFuture`/`BoxStream` 与内联泛型实现的分发成本。
///
/// # 背景说明（Why）
/// - `spark-core` 的对象安全 Trait（如 `TransportFactory::connect`、`ClusterMembership::subscribe`）统一返回 `BoxFuture`/`BoxStream`，
///   以支持在运行时通过 Trait 对象组合协议与集群实现。
/// - 该设计隐含一次虚函数分发 + 堆分配开销，需要量化其在典型高并发场景下的性能影响，并与假想的泛型/GAT 实现进行对比。
///
/// # 测试逻辑（How）
/// - `measure_box_future`/`measure_inline_future`：模拟并发建连场景，每次轮询一个立即就绪的 Future，统计累计耗时。
/// - `measure_box_stream`/`measure_inline_stream`：模拟订阅事件流场景，消费固定长度的就绪 Stream 序列。
/// - 所有测试均使用手写的 `noop` Waker 与 `Instant` 计时，避免引入额外依赖或调度噪声。
/// - `--quick` 模式减少迭代次数，用于 CI 烟囱验证；默认模式扩大数量级以获取稳定统计结果。
///
/// # 输出（What）
/// - 每类测试打印总耗时纳秒与单次操作平均耗时（纳秒）。
/// - 同时输出 Box 版本相对于内联版本的倍率，便于直接纳入文档描述。
///
/// # 注意事项（Trade-offs & Gotchas）
/// - 该测试主要衡量 CPU 侧开销，不涉及实际 IO 或执行器调度，因此真实工作负载下的总体影响会更低。
/// - 若未来需要评估不同编译优化（LTO、ThinLTO），可在 CI 中扩展运行矩阵；当前实现保证无额外依赖，易于集成。
fn main() {
    let is_quick = env::args().skip(1).any(|arg| arg == "--quick");
    let future_iterations = if is_quick { 200_000_u64 } else { 2_000_000_u64 };
    let stream_iterations = if is_quick { 50_000_u64 } else { 500_000_u64 };

    let future_inline = measure_inline_future(future_iterations);
    let future_boxed = measure_box_future(future_iterations);
    let stream_inline = measure_inline_stream(stream_iterations);
    let stream_boxed = measure_box_stream(stream_iterations);

    report("future_inline", future_inline, future_iterations);
    report("future_boxed", future_boxed, future_iterations);
    report("stream_inline", stream_inline, stream_iterations);
    report("stream_boxed", stream_boxed, stream_iterations);

    println!(
        "future_overhead_ratio={:.3}",
        ratio(future_boxed, future_inline)
    );
    println!(
        "stream_overhead_ratio={:.3}",
        ratio(stream_boxed, stream_inline)
    );
}

/// 打印基准结果的辅助函数。
fn report(label: &str, duration: Duration, iterations: u64) {
    let per_iter = duration.as_nanos() as f64 / iterations as f64;
    println!("{label}_elapsed_ns={}", duration.as_nanos());
    println!("{label}_per_iter_ns={per_iter:.4}");
}

/// 计算两个耗时的倍率。
fn ratio(a: Duration, b: Duration) -> f64 {
    a.as_secs_f64() / b.as_secs_f64()
}

/// 构造一个永不唤醒任务的空 Waker，用于同步轮询基准。
fn noop_waker() -> Waker {
    unsafe fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(core::ptr::null(), &VTABLE)
    }
    unsafe fn wake(_: *const ()) {}
    unsafe fn wake_by_ref(_: *const ()) {}
    unsafe fn drop(_: *const ()) {}
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
    unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) }
}

/// 手写立即就绪的 Future，模拟 `TransportFactory::connect` 返回的短生命周期任务。
struct ReadyFuture {
    payload: u64,
}

static FUTURE_ACCUM: AtomicU64 = AtomicU64::new(0);

impl ReadyFuture {
    fn new(payload: u64) -> Self {
        Self { payload }
    }
}

impl std::future::Future for ReadyFuture {
    type Output = u64;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let value = self.payload;
        FUTURE_ACCUM.fetch_add(value, Ordering::Relaxed);
        cx.waker().wake_by_ref();
        self.payload = 0;
        Poll::Ready(value)
    }
}

/// 就绪 Stream，用于模拟 `ClusterMembership::subscribe` 返回的事件流水。
struct ReadyStream {
    remaining: u64,
}

static STREAM_ACCUM: AtomicU64 = AtomicU64::new(0);

impl ReadyStream {
    fn new(remaining: u64) -> Self {
        Self { remaining }
    }
}

impl Stream for ReadyStream {
    type Item = u64;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.remaining == 0 {
            Poll::Ready(None)
        } else {
            self.remaining -= 1;
            STREAM_ACCUM.fetch_add(self.remaining, Ordering::Relaxed);
            cx.waker().wake_by_ref();
            Poll::Ready(Some(self.remaining))
        }
    }
}

fn measure_inline_future(iterations: u64) -> Duration {
    FUTURE_ACCUM.store(0, Ordering::Relaxed);
    let waker = noop_waker();
    let started = Instant::now();
    let mut checksum = 0_u64;
    for idx in 0..iterations {
        let mut future = ReadyFuture::new(idx);
        let mut cx = Context::from_waker(&waker);
        match Pin::new(&mut future).poll(&mut cx) {
            Poll::Ready(value) => checksum = checksum.wrapping_add(value),
            Poll::Pending => unreachable!("ready future must not pend"),
        }
    }
    let elapsed = started.elapsed();
    black_box(checksum);
    elapsed
}

fn measure_box_future(iterations: u64) -> Duration {
    FUTURE_ACCUM.store(0, Ordering::Relaxed);
    let waker = noop_waker();
    let started = Instant::now();
    let mut checksum = 0_u64;
    for idx in 0..iterations {
        let mut future: BoxFuture<'static, u64> = Box::pin(ReadyFuture::new(idx));
        let mut cx = Context::from_waker(&waker);
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => checksum = checksum.wrapping_add(value),
            Poll::Pending => unreachable!("boxed ready future must not pend"),
        }
    }
    let elapsed = started.elapsed();
    black_box(checksum);
    elapsed
}

fn measure_inline_stream(iterations: u64) -> Duration {
    STREAM_ACCUM.store(0, Ordering::Relaxed);
    let waker = noop_waker();
    let started = Instant::now();
    let mut checksum = 0_u64;
    let mut stream = ReadyStream::new(iterations);
    loop {
        let mut cx = Context::from_waker(&waker);
        match Pin::new(&mut stream).poll_next(&mut cx) {
            Poll::Ready(Some(value)) => checksum = checksum.wrapping_add(value),
            Poll::Ready(None) => break,
            Poll::Pending => unreachable!("ready stream must not pend"),
        }
    }
    let elapsed = started.elapsed();
    black_box(checksum);
    elapsed
}

fn measure_box_stream(iterations: u64) -> Duration {
    STREAM_ACCUM.store(0, Ordering::Relaxed);
    let waker = noop_waker();
    let started = Instant::now();
    let mut checksum = 0_u64;
    let mut stream: BoxStream<'static, u64> = Box::pin(ReadyStream::new(iterations));
    loop {
        let mut cx = Context::from_waker(&waker);
        match stream.as_mut().poll_next(&mut cx) {
            Poll::Ready(Some(value)) => checksum = checksum.wrapping_add(value),
            Poll::Ready(None) => break,
            Poll::Pending => unreachable!("boxed ready stream must not pend"),
        }
    }
    let elapsed = started.elapsed();
    black_box(checksum);
    elapsed
}
