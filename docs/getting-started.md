# Spark 2026 最小上手路径

> **目标：** 帮助第一次接触 Spark 2026 套件的同学，在 30 分钟内跑通端到端最小例，同时理解核心名词、目录与常用命令。

## 1. 前置依赖

| 依赖 | 说明 |
| --- | --- |
| Rust 1.89.0 | 仓库根目录的 `rust-toolchain.toml` 已锁定版本；安装 `rustup` 后自动切换。 |
| Git 2.39+ | 拉取代码与提交变更。 |
| `make` (可选) | 运行 `make ci-*` 系列质量检查时更方便。 |

首次进入仓库后执行一次 `rustup component add rustfmt clippy`，确保格式化与静态检查可用。

## 2. 仓库速览

```
.
├── crates/
│   ├── spark-core/                 # 框架核心契约与稳定 API
│   ├── spark-codec-line/           # 演示用行分隔文本编解码扩展
│   └── spark-transport-tcp/        # 典型传输实现示例之一
├── docs/                # 设计说明、规范与操作指南
└── snapshots/           # 标准化样例数据
```

推荐先阅读下方最小例，再按需查阅 `docs/` 中的专题文档（并关注「[常见陷阱](./pitfalls.md)」）。

## 3. 10 分钟跑通最小例

本仓库提供 `crates/spark-codec-line` 扩展中的 `minimal` 样例，演示如何在自定义缓冲池上复用 `spark-core` 的编解码契约。

1. **拉取依赖并编译：**
   ```bash
   cargo check --workspace
   ```
2. **运行最小例：**
   ```bash
   cargo run -p spark-codec-line --example minimal
   ```
3. **预期输出：**
   ```text
   [spark-codec-line/minimal] encoded=12 bytes, decoded="hello spark"
   ```

> 若输出与预期不符，请先确认 Rust 版本与 `cargo` 命令均来自 `rustup` 安装的稳定通道。

### 3.1 样例代码结构

- 入口文件：`crates/spark-codec-line/examples/minimal.rs`
- 核心要点：
  - 用 `SimpleBufferPool` 构造 `EncodeContext`/`DecodeContext`，演示最小依赖注入；
  - 实例化 `LineDelimitedCodec` 并完成一次编码/解码往返；
  - 所有 Mock 类型均带有“教案级”注释，解释为何以及如何实现。

### 3.2 进一步探索

- 修改 `payload` 字符串并重新运行，观察预算检查如何响应长消息；
- 将 `SimpleBufferPool::statistics` 增加打印，理解监控指标的最小需求；
- 对照 `docs/pitfalls.md`，规避在真实项目落地时的十大陷阱。

## 4. 常用质量检查

`make ci-*` 系列命令封装了核心检查，以下为常用组合：

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace --no-default-features --features alloc
cargo doc --workspace
cargo bench --workspace -- --quick
make ci-lints
make ci-zc-asm
make ci-no-std-alloc
make ci-doc-warning
make ci-bench-smoke
```

建议在提交前至少运行 `cargo fmt`、`cargo clippy` 与 `cargo test`，避免 CI 失败。

## 5. 求助与反馈

- 项目结构、术语不清楚：先查阅 `docs/global-architecture.md` 与 `docs/dual-layer-interface-matrix.md`。
- 样例或文档问题：提 Issue 时附带命令输出与环境信息，便于复现。
- 对「最小上手路径」有优化建议：欢迎直接在 PR 中补充经验或提交改进版本。

祝使用顺利！
