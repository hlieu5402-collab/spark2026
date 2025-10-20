#!/usr/bin/env bash
set -euo pipefail

# == 一致性护栏脚本（教案级注释） ==
#
# ## 意图 (Why)
# 1. 防止历史上曾出现过的“第二套 PollReady/BackpressureReason”定义重新混入代码库，
#    破坏跨 crate 的状态表达一致性，进而导致调用方在面对背压信号时出现歧义或重复实现。
# 2. 将守护逻辑固化在 CI 中，确保每一次提交都会自动巡检，而不是依赖人工 code review
#    记忆规则，降低漏检风险。
#
# ## 所在位置与架构作用 (Where)
# - 脚本位于 `tools/ci/`，作为 CI Job 的前置门禁，在其他构建、测试任务之前快速失败，
#   以节省资源并为贡献者提供即时反馈。
# - 该脚本与 `make ci-*` 等命令互补：`make` 主要覆盖编译/测试，当前脚本负责语义一致性。
#
# ## 核心策略 (How)
# - 使用 `ripgrep`/`git ls-files` 精准定位潜在违规：
#   1. 禁止出现新的 `enum PollReady` 定义，避免重复的就绪状态枚举。
#   2. 若检测到旧的 `BackpressureReason` 关键词，立即失败，保证仓库内不存在第二套背压原因实现。
#   3. 禁止新增 `backpressure*.rs` 顶层文件名（保留内部模块目录结构），防止模块命名分叉。
# - 每一项检查均返回详细的错误提示，协助开发者理解违规原因和修复建议。
#
# ## 契约 (What)
# - **输入**：无显式参数，脚本基于当前 Git 仓库状态运行，默认在 CI/本地仓库根目录执行。
# - **输出**：
#   - 成功：退出码 0，且无输出；
#   - 失败：标准错误输出违规明细，退出码非 0。
# - **前置条件**：
#   - 仓库必须初始化 Git，且已安装 `git`、`rg`；
#   - 需要在 Unix shell 环境下运行（使用 Bash）。
# - **后置条件**：若发现违规，脚本立即通过非零退出码阻断后续 CI 步骤。
#
# ## 设计考量与权衡 (Trade-offs)
# - 直接使用正则匹配可以快速覆盖大部分情况，但如果未来出现宏生成代码，需要额外 guard。
# - 为彻底移除旧实现，我们直接禁止任何形式的 `BackpressureReason` 字面量出现，
#   一旦迁移完毕无需再维护可见性 allowlist，降低脚本复杂度。
# - 文件名检查采用 allowlist（允许位于目录 `*/backpressure/` 内的内部模块），
#   可以与现有结构兼容，但如需重构文件布局需调整 allowlist。
#
# ## 风险提醒 (Gotchas)
# - 若开发者在未安装 `ripgrep` 的环境运行脚本，将收到缺少命令的错误；CI 镜像已预装。
# - 由于使用 `git ls-files`，未跟踪文件不会被检测到；提交前请确保已 `git add`。

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)
cd "$REPO_ROOT"

violation_count=0

## 检查一：禁止新的 `enum PollReady`
# - **意图**：保持唯一的就绪状态枚举定义。
# - **实现逻辑**：
#   1. 使用 `rg` 在所有 Rust 源文件中查找 `enum PollReady`。
#   2. 若命中，记录文件位置并输出修复指引。
# - **后置条件**：无违规时保持静默。
check_forbidden_poll_ready() {
    local matches
    mapfile -t matches < <(rg --color=never --pcre2 --glob '*.rs' -n '\benum\s+PollReady\b' || true)
    if ((${#matches[@]} > 0)); then
        printf '错误：检测到被禁止的 `enum PollReady` 定义，禁止引入第二套就绪状态枚举。\n' >&2
        printf '位置：\n' >&2
        printf '  %s\n' "${matches[@]}" >&2
        printf '建议：请统一使用 `status::ready` 模块中的现有定义。\n' >&2
        violation_count=1
    fi
}

## 检查二：禁止遗留的 `BackpressureReason`
# - **意图**：迁移完成后彻底清除旧的背压枚举，防止贡献者从历史提交中复制粘贴旧实现。
# - **实现逻辑**：若仓库仍出现 `BackpressureReason` 字面量，立即报告违规。
# - **契约**：允许在 Markdown 文档中保留历史记录，但要求在 Rust 源码中完全消除。
check_forbidden_backpressure_reason() {
    local matches
    mapfile -t matches < <(rg --color=never --pcre2 --glob '*.rs' -n '\bBackpressureReason\b' || true)
    if ((${#matches[@]} > 0)); then
        printf '错误：检测到遗留的 `BackpressureReason` 关键词，请彻底移除旧的背压枚举实现。\n' >&2
        printf '位置：\n' >&2
        printf '  %s\n' "${matches[@]}" >&2
        printf '建议：统一使用 `status::ready` 模块中的 `ReadyState/BusyReason`。\n' >&2
        violation_count=1
    fi
}

## 检查三：禁止新增 `backpressure*.rs` 顶层文件
# - **意图**：防止再次出现平行的背压模块命名，保持模块拓扑唯一。
# - **实现逻辑**：
#   1. 利用 `git ls-files` 枚举所有匹配 `backpressure*.rs` 的已跟踪文件。
#   2. 允许位于目录 `*/backpressure/` 内的内部模块（通常为私有实现细分目录）。
#   3. 其余命名视为违规，提示改用已有模块或内部 mod 结构。
# - **风险提示**：若未来调整目录结构，需要同步更新 allowlist。
check_backpressure_filenames() {
    local files
    mapfile -t files < <(git ls-files '*backpressure*.rs')
    for file in "${files[@]}"; do
        [[ -z "$file" ]] && continue
        if [[ "$file" == */backpressure/* ]]; then
            continue
        fi
        printf '错误：检测到受限文件命名 `%s`，请勿新增顶层 `backpressure*.rs`。\n' "$file" >&2
        printf '建议：如需扩展，请在已有模块目录下创建子模块或复用现有实现。\n' >&2
        violation_count=1
    done
}

check_forbidden_poll_ready
check_forbidden_backpressure_reason
check_backpressure_filenames

if ((violation_count > 0)); then
    exit 1
fi
