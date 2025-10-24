//! smol 运行时适配层的 TCK 驱动入口。
//!
//! # 教案式摘要
//! - **背景 (Why)**：smol 以极简执行器著称，常用于嵌入式或 CLI 场景；通过本测试确保在轻量运行时下也能满足
//!   Spark 核心契约。
//! - **定位 (Architecture)**：独立的测试 crate，供 CI runtime matrix 调用，与其它运行时实现共享相同的 TCK
//!   执行逻辑。
//! - **方案 (How)**：使用 `smol::block_on` 手动驱动异步执行器，并在其中调用同步的 TCK 套件。

use spark_contract_tests::{
    run_backpressure_suite, run_cancellation_suite, run_errors_suite, run_graceful_shutdown_suite,
    run_hot_reload_suite, run_hot_swap_suite, run_observability_suite, run_state_machine_suite,
};

/// 封装全部 TCK 套件的顺序执行逻辑。
///
/// # 教案级注释
/// - **意图 (Why)**：保持与 Tokio/async-std 版本一致的调用顺序，确保不同运行时的日志与失败定位一致。
/// - **角色 (Architecture)**：隶属于适配层测试的共享逻辑模块，为后续运行时扩展提供模板。
/// - **实现 (How)**：依次调用八个 `run_*_suite` 函数，每个函数失败时会 panic 并附带上下文。
/// - **契约 (What)**：无输入、无输出；成功返回意味着所有套件通过；任何 panic 将向上传播。
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

/// 在 smol 执行器上运行 Spark TCK。
///
/// # 教案级注释
/// - **意图 (Why)**：验证 smol 提供的单线程执行器能够承载 Spark 的契约用例，为资源受限场景提供信心。
/// - **位置 (Architecture)**：runtime matrix 的一个分支，与其它运行时共享测试目录结构。
/// - **过程 (How)**：在常规 `#[test]` 中调用 `smol::block_on` 启动执行器，并在异步上下文中执行顺序函数；
///   由于内部代码为同步逻辑，不会触发 `.await`，因此测试会快速完成。
/// - **契约 (What)**：无输入参数；成功执行后返回 `()`；若 TCK panic 则直接让测试失败。
#[test]
fn smol_runtime_executes_spark_tck() {
    smol::block_on(async {
        run_all_suites_sequentially();
    });
}
