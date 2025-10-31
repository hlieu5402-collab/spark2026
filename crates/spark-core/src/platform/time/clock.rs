// 教案级说明：本模块仅在启用 `std` Feature 后编译。
//
// - **意图 (Why)**：在“纯契约”阶段保留时钟相关的类型与 trait，使调用方仍可依赖统一
//   的 API 约束完成编译，同时移除任何具体实现细节，防止无意间重新引入线程、定时器
//   或调度逻辑。
// - **契约 (What)**：对外暴露 [`Clock`] trait 以及 `SystemClock`、`MockClock` 等占位类型，
//   它们维持原有的签名但不再包含运行时逻辑；实际实现将在后续阶段由宿主运行时补充。
// - **实现提示 (How)**：父模块在 `time/mod.rs` 中使用 `#[cfg(feature = "std")]` 限定该子模
//   块；本文件中所有函数均转化为教案式桩实现（`unimplemented!()` 或空操作），确保不
//   存在 `std::thread`、`sleep` 等痕迹，同时维持文档所需的占位结构。

use alloc::boxed::Box;
use core::marker::PhantomData;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

/// `Sleep` 为时钟接口返回的统一延迟 Future 类型。
///
/// # 设计说明
/// - **意图 (Why)**：保持异步等待的统一形态，便于未来实现通过特化或装箱进行扩展；
/// - **契约 (What)**：返回 `Future<Output = ()>`，必须满足 `Send + 'static`；
/// - **注意 (Trade-offs)**：当前阶段不再提供默认实现，调用方需要自行注入具备实际行为
///   的实现类型。
pub type Sleep = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// 抽象可注入的时钟，实现统一的“获取当前时间”与“等待指定时间”能力。
///
/// # 教案式注释
/// - **意图 (Why)**：通过 trait 限定统一契约，便于替换为宿主运行时的真实实现；
/// - **契约 (What)**：`now` 返回单调时间点，`sleep` 生成延迟 Future；
/// - **注意 (Trade-offs)**：本文件仅保留接口，不提供默认实现或回退逻辑。
pub trait Clock: Send + Sync + 'static {
    /// 返回当前的单调时间点。
    fn now(&self) -> Instant;

    /// 返回一个在指定持续时间后完成的睡眠 Future。
    fn sleep(&self, duration: Duration) -> Sleep;
}

/// `SystemClock` 为生产运行时预留的系统时间占位类型。
///
/// # 教案式注释
/// - **意图 (Why)**：保留类型名称，确保依赖方在迁移到纯契约阶段时无需改动签名；
/// - **契约 (What)**：类型实现 [`Clock`] trait，但所有方法都会在运行期触发 `unimplemented!()`；
/// - **注意 (Trade-offs)**：占位实现不会提供任何时间语义，必须由宿主运行时替换。
#[derive(Clone, Debug, Default)]
pub struct SystemClock {
    _marker: PhantomData<()>,
}

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        let _ = self;
        unimplemented!("SystemClock::now 在纯契约阶段不可用；请由宿主运行时提供具体实现",)
    }

    fn sleep(&self, duration: Duration) -> Sleep {
        let _ = duration;
        unimplemented!("SystemClock::sleep 在纯契约阶段不可用；请由宿主运行时提供具体实现",)
    }
}

/// `MockClock` 为测试场景预留的虚拟时钟占位类型。
///
/// # 教案式注释
/// - **意图 (Why)**：维持兼容的 API，以便现有单元/契约测试在链接阶段即可替换为自定义实现；
/// - **契约 (What)**：保留 `new`/`with_start`/`advance`/`elapsed` 等方法签名，但不再维护状态；
/// - **注意 (Trade-offs)**：本占位实现所有方法都会 panic，用于提醒开发者在集成时补充真实逻辑。
#[derive(Clone, Debug, Default)]
pub struct MockClock {
    _marker: PhantomData<()>,
}

impl MockClock {
    /// 创建起始时间为当前系统时间的虚拟时钟，占位实现忽略真实时间。
    pub fn new() -> Self {
        Self {
            _marker: PhantomData,
        }
    }

    /// 以指定起始时间构造虚拟时钟，占位实现会忽略该参数。
    pub fn with_start(origin: Instant) -> Self {
        let _ = origin;
        Self {
            _marker: PhantomData,
        }
    }

    /// 手动推进虚拟时钟，占位实现仅保证签名存在。
    pub fn advance(&self, delta: Duration) {
        let _ = (self, delta);
        unimplemented!("MockClock::advance 在纯契约阶段不可用；请注入具备虚拟时间能力的实现",)
    }

    /// 返回自起始时间以来的虚拟偏移，占位实现不会提供有效值。
    pub fn elapsed(&self) -> Duration {
        unimplemented!("MockClock::elapsed 在纯契约阶段不可用；请注入具备虚拟时间能力的实现",)
    }
}

impl Clock for MockClock {
    fn now(&self) -> Instant {
        let _ = self;
        unimplemented!("MockClock::now 在纯契约阶段不可用；请注入具备虚拟时间能力的实现",)
    }

    fn sleep(&self, duration: Duration) -> Sleep {
        let _ = duration;
        unimplemented!("MockClock::sleep 在纯契约阶段不可用；请注入具备虚拟时间能力的实现",)
    }
}
