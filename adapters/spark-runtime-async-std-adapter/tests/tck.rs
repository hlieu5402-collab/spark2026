//! async-std 运行时适配层的 TCK 驱动入口。
//!
//! # 教案式摘要
//! - **目标 (Why)**：验证在 async-std 运行时上复用 spark-core 契约时不会引入隐藏依赖或语义差异。
//! - **定位 (Architecture)**：适配层测试 crate，供 CI matrix 针对不同执行器运行，确保核心契约在多运行时下一致。
//! - **方法 (How)**：利用 `async-std` 提供的测试宏启动异步上下文，再调用同步的 TCK 执行函数。

use spark_contract_tests::{
    run_backpressure_suite, run_cancellation_suite, run_errors_suite, run_graceful_shutdown_suite,
    run_hot_reload_suite, run_hot_swap_suite, run_observability_suite, run_state_machine_suite,
};

/// 顺序执行全部 TCK 套件的帮助函数。
///
/// # 教案级注释
/// - **意图 (Why)**：集中封装调用顺序，保证不同运行时的测试实现保持一致，降低维护成本。
/// - **全局关系 (Architecture)**：该函数充当 TCK crate 与各运行时测试入口之间的共享桥梁，
///   逻辑与 Tokio 版本保持一一对应，便于阅读 diff。
/// - **实现 (How)**：直接调用八个 `run_*_suite` 函数；由于这些函数是阻塞式的，调用次序即执行次序。
/// - **契约 (What)**：无参数、无返回值；调用前只需确保 `spark-contract-tests` 可用；如遇失败将 panic。
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

/// 在 async-std 运行时环境中执行 Spark TCK。
///
/// # 教案级注释
/// - **意图 (Why)**：确保即使在基于协程的 async-std 环境中，Spark 契约也能按预期运行，为选择 async-std 的
///   用户提供参考样例。
/// - **位置 (Architecture)**：属于 CI runtime matrix 的一环，与 Tokio/Smol/Glommio 版本共同构成外部适配层的
///   验收门槛。
/// - **流程 (How)**：使用 `#[async_std::test]` 宏启动执行器，并在异步上下文内调用顺序执行函数；由于 TCK 本身
///   为同步逻辑，调用过程不会 yield。
/// - **契约 (What)**：测试函数没有参数；前置条件是 async-std 测试宏自动初始化运行时；后置条件是返回 `()` 或 panic。
#[async_std::test]
async fn async_std_runtime_executes_spark_tck() {
    run_all_suites_sequentially();
}
