# PR 摘要

请在此描述本次变更的整体目标、范围与影响面。

- 受影响的模块或 crate：
- 是否引入公共 API 变更：是 / 否（若为“是”，请在下方填写 CEP 信息）
- [ ] 触碰统一协议公共 API？（若勾选，请在“CEP 关联”章节补充编号与治理结论）

## 测试与验证

请勾选或补充你实际执行的检查命令。确保所有必要的检查均已通过并附上关键输出链接或说明。

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo build --workspace --no-default-features --features alloc`
- [ ] `cargo doc --workspace`
- [ ] `cargo bench --workspace -- --quick`
- [ ] `make ci-lints`
- [ ] `make ci-zc-asm`
- [ ] `make ci-no-std-alloc`
- [ ] `make ci-doc-warning`
- [ ] `make ci-bench-smoke`
- [ ] 其他（请补充）：

> 如因环境限制未执行全部检查，请在上方清晰说明原因及风险评估。

## ReadyState / PollReady / ExecutionContext 变更治理

> 若本次 PR 涉及 `ReadyState` / `PollReady` / `ExecutionContext`，请勾选下列核对项，并在描述或评论中附上对应材料的链接或摘要；若无相关变更可保留未勾选状态。

- [ ] 已补充或更新契约测试，覆盖受影响的 ReadyState / PollReady / ExecutionContext 分支
- [ ] 已同步更新指标与 Runbook，确保观测与运维手册覆盖新语义
- [ ] 已提供本次 `cargo semver-checks` 报告（可为链接或关键结论）

## 变更类型

- [ ] 新功能
- [ ] 缺陷修复
- [ ] 文档更新
- [ ] 性能优化
- [ ] 重构 / 重组
- [ ] 构建 / DevOps
- [ ] 其他（请注明）：

## CEP 关联（公共 API 必填）

- CEP 编号：`CEP-xxxx`
- CEP 文档链接：`docs/governance/CEP-xxxx-*.md`
- 当前状态：草案 / 讨论中 / 已定稿 / 废弃

> **必须**在 CEP 章节中概述方案、风险与迁移策略，并确保相关 Owner 已参与评审。

## 审批清单

- [ ] 已根据 CODEOWNERS 规则请求全部必需审查人
- [ ] 相关 issue / 任务单已关联并同步状态
- [ ] 依赖或后续行动项：

## 风险与回滚

- 风险评估：
- 回滚策略：

## 补充说明

- 设计权衡：
- 兼容性 / 迁移策略：
- 其他需要 reviewer 关注的重点：
