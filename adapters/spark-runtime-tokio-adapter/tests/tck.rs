//! Tokio 运行时适配层的 TCK 驱动入口。
//!
//! # 教案式说明
//! - **定位 (Why)**：Spark 官方不允许 `spark-core` 直接依赖 Tokio，因此实际运行时集成必须在适配层完成；
//!   本测试 crate 作为示例宿主，在 Tokio 多线程执行器上运行官方 TCK，证明契约在该运行时下成立。
//! - **作用 (What)**：编译期依赖 `spark-core` 与 `spark-contract-tests`，运行期通过 Tokio 启动异步上下文，
//!   执行全部 TCK 套件；若任意用例失败，CI 将立即标红。
//! - **方法 (How)**：复用 `spark-contract-tests` 公开的 `run_*_suite` 函数，将其封装为同步帮助函数，再在
//!   `#[tokio::test]` 提供的多线程执行器中调用。

use spark_contract_tests::{
    run_backpressure_suite, run_cancellation_suite, run_errors_suite, run_graceful_shutdown_suite,
    run_hot_reload_suite, run_hot_swap_suite, run_observability_suite, run_state_machine_suite,
};

/// 聚合全部 TCK 套件的执行入口。
///
/// # 教案级注释
/// - **意图 (Why)**：Tokio 测试函数只需调用一次即可触发所有契约验证，避免在运行时层重复维护测试列表。
/// - **位置 (Architecture)**：位于运行时适配层的测试工具模块，充当 `spark-contract-tests` 与 Tokio 执行器之间
///   的胶水代码；核心逻辑仍由上游 crate 提供。
/// - **实现逻辑 (How)**：顺序调用八个主题套件，每个套件内部会进一步迭代实际的断言函数；由于这些函数
///   均为同步接口，因此无需额外的异步桥接或 spawn。
/// - **契约 (What)**：函数不接受参数、不返回值；前置条件是调用方已经正确配置 `spark-core` 依赖；后置条件
///   是若所有套件通过则正常返回，若任一套件失败则直接 panic（由 TCK 自身负责）。
/// - **考量 (Trade-offs)**：顺序执行在多线程环境下不会充分利用并行性，但保证了日志与失败上下文的可读性，
///   更适合作为守门测试使用。
fn run_all_suites_sequentially() {
    run_backpressure_suite();
    run_cancellation_suite();
    run_errors_suite();
    run_graceful_shutdown_suite();
    run_state_machine_suite();
    run_hot_swap_suite();
    run_hot_reload_suite();
    run_observability_suite();
}

/// 在 Tokio 多线程调度器上运行 Spark TCK。
///
/// # 教案级注释
/// - **意图 (Why)**：验证“宿主运行时 + spark-core”组合在 Tokio 上保持契约一致性，为第三方集成提供
///   直接可复用的示例。
/// - **体系位置 (Architecture)**：这是适配层的集成测试入口，由 CI matrix 针对不同运行时重复调用，形成
///   “多运行时回归”防线。
/// - **执行流程 (How)**：借助 `#[tokio::test]` 自动启动多线程运行时；在异步上下文中直接调用同步的
///   `run_all_suites_sequentially`，避免额外的阻塞操作；Tokio 会在测试结束后自动清理资源。
/// - **契约 (What)**：无输入参数；前置条件是 Tokio 测试宏已初始化运行时；后置条件是若所有套件通过
///   则函数返回 `()`，否则 panic。
/// - **权衡 (Trade-offs)**：选择多线程调度器 (`flavor = "multi_thread"`) 以覆盖最常见的部署模式，牺牲少量
///   启动开销换取对跨线程同步原语的验证；若未来需要覆盖 current-thread 模式，可新增测试函数。
#[tokio::test(flavor = "multi_thread")]
async fn tokio_runtime_executes_spark_tck() {
    run_all_suites_sequentially();
}
