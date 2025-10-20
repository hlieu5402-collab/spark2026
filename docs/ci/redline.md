# CI 红线矩阵与 MSRV 固化说明

> 目标：通过固定编译器版本与强制执行关键质量检查，确保任何合并前的修改都在同一红线标准下验证。

## 1. 硬性约束一览

| 阶段 | 对应工作流 Job | 固定工具链 | 核心命令 | 失败策略 |
| --- | --- | --- | --- | --- |
| 代码风格与文档 | `Lints & docs (MSRV)` | `rustc 1.89.0`（`rustfmt`、`clippy`） | `make ci-lints`、`cargo doc --workspace --no-deps` | 任一命令非 0 即 fail |
| 构建矩阵 | `Builds & benches (MSRV)` | `rustc 1.89.0` | `make ci-zc-asm`、`make ci-no-std-alloc`、`make ci-doc-warning`、`make ci-bench-smoke` | 任一命令非 0 即 fail |
| 语义版本兼容性 | `SemVer compatibility (MSRV)` | `rustc 1.89.0` | `cargo semver-checks --manifest-path spark-core/Cargo.toml --baseline-root spark-core` | 检测到破坏性变更即 fail |
| 许可证与安全 | `License & advisory audit` | `rustc 1.89.0` | `cargo deny check advisories bans licenses sources` | 触发 deny 规则即 fail |
| Miri 抽样 | `Miri smoke tests` | `nightly-2024-12-31`（含 `miri`） | `cargo +nightly-2024-12-31 miri setup/test` | 任一命令非 0 即 fail |
| Loom 抽样 | `Loom model checks (MSRV)` | `rustc 1.89.0` | `RUSTFLAGS="--cfg loom" cargo test --features loom-model,std --lib --tests` | 任一命令非 0 即 fail |

- **全部 Job 必须通过**，GitHub Actions 才允许合并。
- **MSRV 固化**：所有主干构建与兼容性检测均在 `rustc 1.89.0` 上运行；涉及解释器（Miri）使用固定 nightly 快照以规避上游波动。

## 2. 运行细节与裁剪原则

1. `make ci-lints`
   - 顺序执行 `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo run --quiet --package spark-deprecation-lint`。
   - 任何警告升级为错误，确保提交物保持零警告状态。
2. 文档检查
   - `cargo doc --workspace --no-deps` 用于快速验证公开 API 文档是否可生成。
   - `make ci-doc-warning` 再次执行完整文档构建，配合 `RUSTDOCFLAGS=-Dwarnings` 捕捉依赖引入的警告。
3. 构建矩阵
   - `make ci-zc-asm`：常规构建，覆盖默认特性组合。
   - `make ci-no-std-alloc`：校验 `alloc` 配置，避免误用 `std`。
   - `make ci-bench-smoke`：`cargo bench -- --quick`，验证基准代码最小可运行。
4. 语义版本校验
   - 当前版本同时作为基线与对比，确保 API 面在变更周期内自洽；更新基线时需同步刷新 `snapshots` 目录或指向新标签。
5. Miri / Loom 抽样
   - Miri 依赖 nightly，固定 `nightly-2024-12-31`，并以 `cargo miri setup` 预编译运行时。
   - Loom 使用 `LOOM_MAX_PREEMPTIONS=2` 控制状态空间，避免 CI 超时。
6. 许可证与漏洞审计
   - `deny.toml` 拒绝所有未知许可证，并将 `ring` 的复合许可显式白名单化。
   - 任何来自 crates.io 之外的源都需在 `[sources.allow-git]` 中登记，否则默认拒绝。

## 3. 本地自检速查

```bash
# 保持与 CI 一致的顺序，定位问题更快速
make ci-lints
cargo doc --workspace --no-deps
make ci-zc-asm
make ci-no-std-alloc
make ci-doc-warning
make ci-bench-smoke
cargo semver-checks --manifest-path spark-core/Cargo.toml --baseline-root spark-core
cargo deny check advisories bans licenses sources
# 额外抽样（可选，遇到差异请按照 CI 的 nightly / cfg 设置）
cargo +nightly-2024-12-31 miri setup
cargo +nightly-2024-12-31 miri test --manifest-path spark-core/Cargo.toml --features std
RUSTFLAGS="--cfg loom" LOOM_MAX_PREEMPTIONS=2 \
  cargo test --manifest-path spark-core/Cargo.toml --features loom-model,std --lib --tests
```

## 4. 维护提示

- **Rust 版本升级流程**：先在 `rust-toolchain.toml` 与工作流同步修改版本，再更新此文档。
- **新增子 crate**：请在 `semver` Job 中补充对应的 `--manifest-path`，或编写脚本遍历所有对外发布的 crate。
- **许可证变更**：若引入新许可，需在 `deny.toml` 的 `allow` 或 `exceptions` 中登记，并附带理由。
- **抽样策略**：
  - Miri 与 Loom 都采用“冒烟”模式；若未来引入更全面的模型，请在此文档中补充运行参数与耗时估计。
  - 当模型枚举时间超过 10 分钟，请拆分更细粒度的测试或降低预emption。

通过以上约束，CI 将作为质量红线，确保合入主干的每一条变更都在统一的 MSRV 与合规矩阵下验证。
