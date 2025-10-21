#!/usr/bin/env bash
set -euo pipefail

# == 一致性护栏脚本（教案级注释） ==
#
# ## 意图 (Why)
# 1. 防止历史上曾出现过的“第二套 PollReady/Backpressure Reason”定义重新混入代码库，
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
#   1. 禁止出现新的「PollReady 枚举」定义，避免重复的就绪状态枚举。
#   2. 若检测到旧的 `Backpressure` `Reason` 关键词拼接，立即失败，保证仓库内不存在第二套背压原因实现。
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
# - 为彻底移除旧实现，我们直接禁止任何形式的 `Backpressure` + `Reason` 字面量出现，
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

## 检查一：禁止新的 PollReady 枚举
# - **意图**：保持唯一的就绪状态枚举定义。
# - **实现逻辑**：
#   1. 使用 `rg` 在所有 Rust 源文件中查找 “`enum` + `PollReady`” 组合。
#   2. 若命中，记录文件位置并输出修复指引。
# - **后置条件**：无违规时保持静默。
check_forbidden_poll_ready() {
    local matches
    mapfile -t matches < <(rg --color=never --pcre2 --glob '*.rs' -n '\benum\s+PollReady\b' || true)
    if ((${#matches[@]} > 0)); then
        printf '错误：检测到被禁止的 `enum` `PollReady` 组合定义，禁止引入第二套就绪状态枚举。\n' >&2
        printf '位置：\n' >&2
        printf '  %s\n' "${matches[@]}" >&2
        printf '建议：请统一使用 `status::ready` 模块中的现有定义。\n' >&2
        violation_count=1
    fi
}

## 检查二：禁止遗留的 Backpressure Reason 关键字
# - **意图**：迁移完成后彻底清除旧的背压枚举，防止贡献者从历史提交中复制粘贴旧实现。
# - **实现逻辑**：若仓库仍出现 “Backpressure” 与 “Reason” 无缝拼接的字面量，立即报告违规。
# - **契约**：允许在 Markdown 文档中保留历史记录，但要求在 Rust 源码中完全消除。
check_forbidden_backpressure_reason() {
    local matches
    local legacy_token="Backpressure""Reason"
    mapfile -t matches < <(rg --color=never --pcre2 --glob '*.rs' -n "\\b${legacy_token}\\b" || true)
    if ((${#matches[@]} > 0)); then
        printf '错误：检测到遗留的 `%s%s` 关键词，请彻底移除旧的背压枚举实现。\n' 'Backpressure' 'Reason' >&2
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

## 检查四：禁止公共 API 暴露第二套就绪/背压命名
# - **意图 (Why)**：
#   1. 确保 `ReadyState/ReadyCheck/BusyReason` 仍然是唯一且权威的语义出口，
#      避免贡献者以 `ReadyCheck2`、`BusyCode` 等命名包装同一语义，破坏跨 crate 一致性。
#   2. 一旦公共接口暴露了第二套命名，下游调用方在升级依赖后将面临“哪套类型才是准绳”
#      的二义性，本检查通过 CI 直接阻断这类分叉。
# - **所在位置与角色 (Where)**：位于一致性脚本中，紧跟前序结构化检查，确保在编译/测试之前
#   就能捕获命名违规，降低回滚成本。
# - **执行逻辑 (How)**：
#   1. 使用正则匹配 `pub struct/enum/type/trait/mod` 及 `pub use` 语句，收集所有对外可见的标识符；
#   2. 将名称与禁止列表进行对比：
#      - 若以 `ReadyCheck/ReadyState/BusyReason` 为前缀但带有额外后缀（表示试图派生“第二版”）；
#      - 或命名包含 `BusyCode`、`Backpressure*` 等历史遗留标签；
#   3. 命中后打印文件及建议，提示回归统一的 `status::ready` 抽象。
# - **契约与边界 (What)**：
#   - **输入**：仓库内所有受 Git 管理的 `.rs` 文件；
#   - **输出**：若发现违规，输出详细位置与整改建议；
#   - **前置条件**：`rg` 可用；
#   - **后置条件**：一旦检测到命名分叉即设置 `violation_count`，阻断后续流程。
# - **设计取舍 (Trade-offs)**：
#   - 通过“正则 + allowlist”方案快速覆盖 80% 以上风险，避免引入 AST 解析成本；
#   - 仅关注公开可见项（`pub` 开头），既保证精准，又允许内部实现自由演进。
check_public_ready_naming() {
    local definition_pattern
    local use_pattern
    local definition_matches
    local use_matches

    # 正则说明：
    # - `pub(?:\s+|\([^)]*\)|/\*.*?\*/)*` 允许 `pub`, `pub(crate)` 及行内注释；
    # - `struct|enum|trait|type|mod` 捕获对外可见的类型/模块定义；
    # - `ReadyCheck[A-Za-z0-9_]+` 等限定至少带一个后缀，排除合法基准名。
    definition_pattern='^\s*pub(?:\s+|\([^)]*\)\s+|/\*.*?\*/\s*)*(struct|enum|trait|type|mod)\s+(?:ReadyCheck[A-Za-z0-9_]+|ReadyState[A-Za-z0-9_]+|BusyReason[A-Za-z0-9_]+|BusyCode\b|Backpressure[A-Za-z0-9_]+)'
    use_pattern='^\s*pub\s+use\b[^;]*\b(?:ReadyCheck[A-Za-z0-9_]+|ReadyState[A-Za-z0-9_]+|BusyReason[A-Za-z0-9_]+|BusyCode\b|Backpressure[A-Za-z0-9_]+)\b'

    mapfile -t definition_matches < <(rg --color=never --pcre2 --no-heading -n --glob '*.rs' "$definition_pattern" || true)
    mapfile -t use_matches < <(rg --color=never --pcre2 --no-heading -n --glob '*.rs' "$use_pattern" || true)

    if ((${#definition_matches[@]} > 0 || ${#use_matches[@]} > 0)); then
        printf '错误：检测到对外暴露的第二套 Ready/Busy/Backpressure 命名，请回归 `status::ready` 提供的统一抽象。\n' >&2
        printf '位置：\n' >&2
        if ((${#definition_matches[@]} > 0)); then
            printf '  %s\n' "${definition_matches[@]}" >&2
        fi
        if ((${#use_matches[@]} > 0)); then
            printf '  %s\n' "${use_matches[@]}" >&2
        fi
        printf '建议：直接复用 `ReadyState/ReadyCheck/BusyReason`，或在私有模块内进行转换，避免面向调用方暴露新命名。\n' >&2
        violation_count=1
    fi
}

## 检查五：禁止将 `ReadyState::BudgetExhausted` 映射为 `Busy(...)`
# - **意图 (Why)**：
#   1. `BudgetExhausted` 表示硬性额度耗尽，属于拒绝请求的终止态；若被映射为 `Busy`，
#      会误导调用方继续重试，造成级联退化。
#   2. 历史上曾出现“BudgetExhausted → Busy” 的折衷实现，本检查在 CI 层面彻底封堵该做法。
# - **所在位置 (Where)**：延续一致性脚本的防线，针对业务语义的关键分支提供额外保护。
# - **执行策略 (How)**：
#   1. 通过 Python 脚本遍历所有受 Git 管理的 `.rs` 文件，逐行分析；
#   2. 仅当命中的行属于代码（跳过注释）时，向后检查最多 5 行，寻找 `ReadyState::Busy` 或 `Busy(` 构造；
#   3. 一旦发现“预算耗尽”分支后立即构造繁忙状态，记录违规位置，提示返回 `ReadyState::BudgetExhausted`。
# - **契约 (What)**：
#   - **输入**：仓库内的 Rust 源文件；
#   - **输出**：若存在违规映射，列出 `起始行 → 目标行` 的对应关系；
#   - **前置条件**：环境需提供 `python3`；
#   - **后置条件**：发现任一违规即终止后续流程。
# - **权衡 (Trade-offs)**：
#   - 采用“窗口检测”而非 AST 解析，兼顾实现复杂度与准确率；
#   - 5 行窗口可覆盖常见 `match`/`if let` 写法，同时避免对无关 `Busy` 分支的误报。
# - **边界提醒 (Gotchas)**：若未来出现生成代码或更复杂的宏展开发生映射，需要扩展检测逻辑。
check_budget_exhausted_to_busy() {
    local python_output

    python_output=$(python3 - <<'PY'
import subprocess
import sys
from pathlib import Path

try:
    files = subprocess.check_output(["git", "ls-files", "*.rs"], text=True).splitlines()
except subprocess.CalledProcessError as exc:  # pragma: no cover - Git 必须可用
    sys.stderr.write(f"无法枚举 Rust 文件：{exc}\n")
    sys.exit(1)

violations = []

for rel_path in files:
    if not rel_path:
        continue

    path = Path(rel_path)
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError as exc:  # pragma: no cover - 文件读取异常直接失败
        sys.stderr.write(f"读取文件失败 {rel_path}: {exc}\n")
        sys.exit(1)

    for idx, line in enumerate(lines):
        if "ReadyState::BudgetExhausted" not in line:
            continue

        if line.lstrip().startswith("//"):
            continue

        # 检查同一行是否立即将 BudgetExhausted 映射为 Busy。
        if "ReadyState::Busy" in line or "Busy(" in line:
            snippet = line.strip()
            violations.append(f"{rel_path}:{idx + 1}:{snippet}")
            continue

        window_end = min(len(lines), idx + 6)
        for look_ahead in range(idx + 1, window_end):
            neighbor = lines[look_ahead]
            if neighbor.lstrip().startswith("//"):
                continue

            if "ReadyState::Busy" in neighbor or "Busy(" in neighbor:
                snippet = neighbor.strip()
                violations.append(
                    f"{rel_path}:{idx + 1}->{look_ahead + 1}:{snippet}"
                )
                break

if violations:
    for item in violations:
        print(item)
PY
) || true

    if [[ -n "$python_output" ]]; then
        printf '错误：检测到将 `ReadyState::BudgetExhausted` 映射为 `Busy(...)` 的实现，违背预算耗尽语义。\n' >&2
        printf '位置（BudgetExhausted 行 → Busy 行）：\n' >&2
        printf '  %s\n' "$python_output" >&2
        printf '建议：在预算耗尽时直接返回 `ReadyState::BudgetExhausted`，并让上层决定是否降级或拒绝请求。\n' >&2
        violation_count=1
    fi
}

check_forbidden_poll_ready
check_forbidden_backpressure_reason
check_backpressure_filenames
check_public_ready_naming
check_budget_exhausted_to_busy

if ((violation_count > 0)); then
    exit 1
fi
