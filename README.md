# Spark 2026

[📘 最小上手路径](docs/getting-started.md) · [⚠️ 十大常见陷阱](docs/pitfalls.md)

Spark 2026 是面向高性能、协议无关场景的异步通信契约集合。本仓库同时提供核心接口 (`spark-core`) 与示例扩展 (`spark-codec-line`)，帮助团队验证兼容性与扩展能力。

## 仓库亮点

- **契约优先：** 通过 `spark-core` 定义传输、路由、配置、运行时等核心接口，支持 `no_std + alloc` 场景。
- **可扩展样例：** `spark-codec-line` 演示如何在不修改核心库的前提下实现行分隔文本编解码器。
- **文档完备：** `docs/` 下包含治理、观测、配置、异步调度等专题说明，辅以本次新增的快速上手与避坑指南。

## 快速开始

1. 安装 Rust 1.89（`rustup` 会根据 `rust-toolchain.toml` 自动选择版本）。
2. 运行 `cargo check --workspace` 确认依赖齐全。
3. 按照 [最小上手路径](docs/getting-started.md) 执行 `cargo run -p spark-codec-line --example minimal`，体验端到端往返流程。
4. 阅读 [十大常见陷阱](docs/pitfalls.md)，避免在生产化过程中踩坑。

## 贡献指南

- 在提交前确保本地通过以下命令：
  ```bash
  cargo fmt --all --check
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test --workspace
  ```
- 若涉及文档或契约更新，请同步维护 `docs/` 目录与相关样例。
- 遇到问题可在 Issue 中附带复现步骤、命令输出与环境信息，便于定位。

欢迎贡献新扩展、运行时实现或经验总结！
