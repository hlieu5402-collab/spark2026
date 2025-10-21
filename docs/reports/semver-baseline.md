# 2025-02 SemVer 基线冒烟报告

## 结论速览
- **检查工具链**：`rustc 1.89.0` + `cargo-semver-checks 0.44.0`。
- **基线来源**：CI 通过 `git worktree` 拉取 `origin/main` 作为对比基线；本地复现时使用 `HEAD^` 等价模拟。
- **结果摘要**：
  - 当前主干相对于上一稳定提交未出现破坏性变更，保持 minor 级别可发布状态。
  - 移除对外 `Service` re-export 的破坏性变更会触发 `trait_missing`，CI 将以 Major 级别阻断合并。

## 正常路径：当前 HEAD vs. 基线 HEAD^
```text
    Building spark-core v0.1.0 (current)
       Built [   2.549s] (current)
     Parsing spark-core v0.1.0 (current)
      Parsed [   0.253s] (current)
    Building spark-core v0.1.0 (baseline)
       Built [  23.789s] (baseline)
     Parsing spark-core v0.1.0 (baseline)
      Parsed [   0.191s] (baseline)
    Checking spark-core v0.1.0 -> v0.1.0 (no change; assume minor)
     Checked [   0.354s] 159 checks: 159 pass, 41 skip
     Summary no semver update required
    Finished [  29.618s] spark-core
```

## 破坏性变更冒烟：移除 `Service` re-export
> 操作步骤：在 PR 分支删除 `pub use service::Service;`，其余保持不变，然后再次执行 `cargo semver-checks`。

```text
    Building spark-core v0.1.0 (current)
       Built [   2.436s] (current)
     Parsing spark-core v0.1.0 (current)
      Parsed [   0.246s] (current)
    Building spark-core v0.1.0 (baseline)
       Built [  23.145s] (baseline)
     Parsing spark-core v0.1.0 (baseline)
      Parsed [   0.189s] (baseline)
    Checking spark-core v0.1.0 -> v0.1.0 (no change; assume minor)
     Checked [   0.369s] 159 checks: 158 pass, 1 fail, 0 warn, 41 skip

--- failure trait_missing: pub trait removed or renamed ---

Description:
A publicly-visible trait cannot be imported by its prior path. A `pub use` may have been removed, or the trait itself may have been renamed or removed entirely.
        ref: https://doc.rust-lang.org/cargo/reference/semver.html#item-remove
       impl: https://github.com/obi1kenobi/cargo-semver-checks/tree/v0.44.0/src/lints/trait_missing.ron

Failed in:
  trait spark_core::Service, previously in file …/spark-core/src/service/traits/generic.rs:30

     Summary semver requires new major version: 1 major and 0 minor checks failed
    Finished [  29.307s] spark-core
```

## 后续动作
- 将本报告归档至 `docs/reports/semver-baseline.md`，供未来回溯。
- CI 已默认执行相同流程：任何破坏性变更都会在 `SemVer compatibility (MSRV)` Job 阻断合并。
