#!/usr/bin/env bash
# 教案级注释：ReadyState / ErrorCategory / ObservabilityContract / BufView 契约同步守门脚本
#
# 目标 (Why)：
# - 核心目的：当 PR 在 `spark-core/src` 中改动 ReadyState、ErrorCategory、ObservabilityContract 或 BufView 契约实现时，强制要求作者同步更新
#   `docs/` 目录下的正式文档或仪表盘说明，确保核心契约变化能第一时间传达到运行与治理团队。
# - 在整体流程中的定位：该脚本作为 CI Lints & docs 阶段的早期守门人，位于编译/测试之前快速阻断缺少文档的契约变更，降低回滚成本。
# - 设计理念：通过 `git diff` 精确定位 `spark-core/src` 目录内涉及关键契约的改动，再校验是否伴随至少一项 `docs/*` 更新，以最小成本获得治理信号。
#
# 逻辑解析 (How)：
# 1. 仅在 Pull Request 事件下执行：push 到主分支的提交已经通过评审，此处无需重复守门。
# 2. 自动解析对比基线：优先使用 PR 基线 SHA；若缺失则回退到目标分支或 `origin/main`，保证 fork 与分支均可运行。
# 3. 限定扫描范围：只分析 `spark-core/src/**/*.rs` 的 diff，过滤测试/文档噪声；随后匹配四个关键契约的标识符。
# 4. 收集触发契约：若 diff 中包含 `ReadyState|ErrorCategory|ObservabilityContract|BufView`，记录涉及的源文件列表。
# 5. 校验文档更新：检查本次提交是否修改了任意 `docs/` 前缀的文件；若缺失则输出详细指引并失败退出。
#
# 契约说明 (What)：
# - 输入：
#   * 环境变量 `GITHUB_EVENT_NAME`、`PR_BASE_SHA`、`GITHUB_BASE_REF`（GitHub Actions 在 PR 步骤中注入）。
# - 前置条件：
#   * 当前工作目录位于仓库根目录，且本地存在 PR Head 以及可访问的 `origin` 远程。
#   * 基线提交必须可解析；脚本会在必要时自动 `git fetch`。
# - 后置条件：
#   * 若检测到契约改动且缺少文档，脚本以非零退出码终止，CI 因此失败。
#   * 若契约改动配套文档齐全或未命中关键契约，脚本静默退出 (0)。
#
# 设计权衡与注意事项 (Trade-offs & Gotchas)：
# - 关键字匹配 VS AST：选择关键字匹配以降低实现复杂度与运行时间；可能出现极少数同名符号导致的“误报”，此时需要人工确认是否应更新文档。
# - 扫描范围：限定 `spark-core/src` 避免测试或外部 crate 的噪声；若未来将契约迁移至其他 crate，需要同步扩展 `SCOPE_PATHSPEC`。
# - 文档检查宽松：仅要求任意 `docs/` 变更，允许作者在最合适的文件中补充说明；治理团队可在评审中进一步确认内容质量。
# - 性能：`git diff` 和 `grep` 的成本极低，脚本不修改工作区，也不会生成临时文件，确保在 CI 中运行稳定。
#
# 风险提示：
# - 若作者以“更新文档在其他 PR 中处理”为由拆分提交，本守门人会拒绝合并；请在同一 PR 中完成契约与文档的同步。
# - 若确实无需文档（罕见），请在 PR 中说明并在评审中获得豁免，但此脚本仍会要求最少的 `docs/` 变更，可通过更新摘要或变更记录满足。
set -euo pipefail

if [[ "${GITHUB_EVENT_NAME:-}" != "pull_request" ]]; then
  exit 0
fi

readonly BASE_SHA_ENV="${PR_BASE_SHA:-}"
readonly BASE_REF_ENV="${GITHUB_BASE_REF:-main}"

resolve_base_commit() {
  local candidate_sha="$1"
  local candidate_ref="$2"

  # 解析顺序：显式 SHA > 目标分支远程引用 > origin/main > 本地 main > HEAD^
  if [[ -n "$candidate_sha" ]]; then
    if ! git rev-parse --verify "$candidate_sha" >/dev/null 2>&1; then
      git fetch origin "$candidate_sha"
    fi
    git rev-parse "$candidate_sha"
    return
  fi

  local remote_ref="refs/remotes/origin/${candidate_ref}"
  if ! git rev-parse --verify "$remote_ref" >/dev/null 2>&1; then
    git fetch origin "${candidate_ref}:${remote_ref}" 2>/dev/null || git fetch origin "$candidate_ref"
  fi

  if git rev-parse --verify "$remote_ref" >/dev/null 2>&1; then
    git rev-parse "$remote_ref"
    return
  fi

  if git rev-parse --verify origin/main >/dev/null 2>&1; then
    git rev-parse origin/main
    return
  fi

  if git rev-parse --verify main >/dev/null 2>&1; then
    git rev-parse main
    return
  fi

  git rev-parse HEAD^
}

readonly BASE_COMMIT="$(resolve_base_commit "$BASE_SHA_ENV" "$BASE_REF_ENV")"

# 限定扫描范围：使用 GIT pathspec 的 glob 语法，仅关注 spark-core/src 下的 Rust 文件。
readonly SCOPE_PATHSPEC=':(glob)spark-core/src/**/*.rs'
readonly CONTRACT_REGEX='\b(ReadyState|ErrorCategory|ObservabilityContract|BufView)\b'

mapfile -t candidate_files < <(git diff "$BASE_COMMIT"...HEAD --name-only -- "$SCOPE_PATHSPEC")
if ((${#candidate_files[@]} == 0)); then
  exit 0
fi

contract_hits=()
for file in "${candidate_files[@]}"; do
  diff_chunk="$(git diff "$BASE_COMMIT"...HEAD -- "$file")"
  if grep -Eq "$CONTRACT_REGEX" <<<"$diff_chunk"; then
    contract_hits+=("$file")
  fi
done

if ((${#contract_hits[@]} == 0)); then
  exit 0
fi

mapfile -t docs_changes < <(git diff "$BASE_COMMIT"...HEAD --name-only -- 'docs/**')
if ((${#docs_changes[@]} > 0)); then
  exit 0
fi

echo "检测到以下契约文件改动但缺少 docs/* 同步：" >&2
for file in "${contract_hits[@]}"; do
  echo "- ${file}" >&2
done

echo "请同步更新 ReadyState / ErrorCategory / ObservabilityContract / BufView 相关文档或仪表盘，并在 PR 中说明具体文件。" >&2
exit 1
