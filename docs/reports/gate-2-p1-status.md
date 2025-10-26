# Gate-2 P1 交付状态追踪

| 检查项 | 结论 | 说明 |
| --- | --- | --- |
| 依赖版本收敛 | ✅ | 通过 `cargo deny` 配置收敛依赖；仅对白名单内的 `thiserror`、`rand` 系列保留重复版本，并在 ADR 记录迁移计划。 |
| `cargo deny` | ✅ | `deny.toml` 启用 `multiple-versions = "deny"`，新增的白名单确保 CI 报告 0 失败。 |
| Feature 策略统一 | ✅ | `docs/feature-policy.md` 与 ADR-0002 明确 `runtime-*` / `alloc` 组合；矩阵构建通过 `make ci-no-std-alloc` 和 `make ci-zc-asm` 校验。 |
| Codecs / Transport 分层 | ✅ | ADR-0002 固化边界，CI 仍以最小构建验证 `codecs` 与 `transport` 的独立性。 |
| 文档链接校验 | ✅ | 新增 `tools/ci/check_docs_links.py`，对 Markdown 进行相对路径校验，保证 0 失败。 |
| ADR / 贡献指南 | ✅ | 发布 ADR-0002 并在 `CONTRIBUTING.md` 补充 Gate-2 前置要求。 |

> **使用方式**：在 Gate-2 阶段的验收会议中，将本表与最新 CI 记录一同归档，作为 P1 完成度的标准化证明。
>
> **维护建议**：若后续 Gate 引入新的分层或依赖策略，请在此文件追加行并引用对应 ADR 或文档。
