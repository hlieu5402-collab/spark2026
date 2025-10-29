# BOOT-1 基线冻结产物

本目录存放 BOOT-1 阶段冻结的可复现基线：

- `cargo-metadata.json`：使用 `cargo metadata --format-version 1 --all-features` 导出的工作区拓扑与依赖信息。
- `cargo-tree.txt`：使用 `cargo tree --workspace --all-features` 导出的依赖树快照。
- `public-api/`：使用 `cargo +nightly public-api -p <crate> --simplified` 为每个公开 crate 抓取的公共 API 列表。
  - 对 `spark-pipeline`、`spark-transport-tcp`、`spark-transport-tls`，生成过程因依赖缺失或语义错误失败，已以 `FAILED` 文本标记并保留标准错误输出。

> 如需更新，请先在新分支中重新执行上述命令，并在追踪 issue 中同步登记。
