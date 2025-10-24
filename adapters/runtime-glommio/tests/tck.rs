//! glommio 运行时适配层的 TCK 驱动入口。
//!
//! # 教案式摘要
//! - **初衷 (Why)**：glommio 采用线程绑核模型，与 Tokio/async-std 的协作式调度不同；通过运行 TCK 验证该模型
//!   下的行为仍符合 spark-core 契约。
//! - **定位 (Architecture)**：此测试 crate 与其它运行时版本一起构成“外部运行时适配层”的验收基线，由 CI matrix
//!   调度执行。
//! - **实现 (How)**：使用 `glommio::LocalExecutorBuilder` 创建单核执行器，并在其异步上下文内运行同步的 TCK 套件。

use glommio::LocalExecutorBuilder;
use spark_contract_tests::{
    run_backpressure_suite, run_cancellation_suite, run_errors_suite, run_graceful_shutdown_suite,
    run_hot_reload_suite, run_hot_swap_suite, run_observability_suite, run_state_machine_suite,
};

/// 汇总所有 TCK 套件的执行入口。
///
/// # 教案级注释
/// - **意图 (Why)**：统一不同运行时测试中的调用顺序，避免维护多份冗余逻辑。
/// - **架构位置 (Architecture)**：作为适配层内部的共享逻辑，与其它运行时版本保持一致，便于 diff 与复用。
/// - **实现细节 (How)**：同步调用八个 `run_*_suite` 函数；每个函数内部负责断言与 panic 处理。
/// - **契约 (What)**：无参数、无返回；前置条件是 `spark-contract-tests` 已被引入；后置条件是所有套件均成功。
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

/// 在 glommio 的线程绑核执行器上运行 Spark TCK。
///
/// # 教案级注释
/// - **意图 (Why)**：确认 Spark 契约在 io_uring 驱动的线程绑核模型下依然成立，为对延迟敏感场景提供官方验证手段。
/// - **体系位置 (Architecture)**：隶属于运行时适配层测试矩阵，补齐与 Tokio/async-std/smol 的对照验证。
/// - **执行步骤 (How)**：
///   1. 使用 `LocalExecutorBuilder::default()` 创建单核执行器；
///   2. 调用 `spawn` 启动异步任务，任务内部直接执行顺序函数；
///   3. 通过 `join()` 阻塞等待任务完成，若任务返回 `Result::Err` 会立即传播。
/// - **契约 (What)**：测试无输入；`spawn` 返回的任务必须成功完成，否则测试失败；执行过程中若 TCK panic，同样会在 join 时抛出。
#[test]
fn glommio_runtime_executes_spark_tck() {
    LocalExecutorBuilder::default()
        .spawn(|| async move {
            run_all_suites_sequentially();
        })
        .expect("glommio 执行器初始化失败")
        .join()
        .expect("glommio 执行器运行 TCK 失败");
}
