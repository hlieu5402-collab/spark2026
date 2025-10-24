#!/usr/bin/env bash
set -euo pipefail

# 教案级注释：脚本用途与设计说明
# 1. 目标 (Why)
#    - Spark 核心强调“运行时无依赖”，因此实际运行时适配需在独立 crate 中验证；
#      本脚本作为 CI 入口，根据传入的运行时名称定位对应的测试 crate，并执行完整 TCK。
# 2. 位置 (Architecture)
#    - 位于 `tools/ci/`，与其它守门脚本并列；由 GitHub Actions 的 runtime matrix 调用，
#      将外部适配层纳入统一的 CI 防线。
# 3. 方法 (How)
#    - 解析运行时参数 → 构造 `adapters/runtime-*/Cargo.toml` 路径 → 调用 `cargo test`；
#      对 glommio 增加 memlock 提示，避免 io_uring 初始化失败。
# 4. 契约 (What)
#    - 输入：单个运行时名称（tokio、async-std、smol、glommio）。
#    - 前置条件：仓库根目录下存在匹配的适配 crate，且可在当前工具链上编译。
#    - 后置条件：若 TCK 全部通过则脚本返回 0；一旦任何测试失败则立即退出非零状态。
# 5. 权衡 (Trade-offs)
#    - 采用简单的 case 分发，便于后续新增运行时；同时在 glommio 场景下执行 memlock 检查，
#      以更友好的错误信息换取少量前置开销。

if [[ $# -ne 1 ]]; then
    cat <<'USAGE' >&2
用法：run_runtime_tck.sh <runtime>
支持的运行时：tokio、async-std、smol、glommio。
USAGE
    exit 1
fi

runtime="$1"
case "${runtime}" in
    tokio|async-std|smol|glommio)
        ;;
    *)
        printf '错误：不支持的运行时 "%s"。仅允许 tokio/async-std/smol/glommio。\n' "${runtime}" >&2
        exit 1
        ;;
esac

REPO_ROOT="$(git rev-parse --show-toplevel)"
MANIFEST="${REPO_ROOT}/adapters/runtime-${runtime}/Cargo.toml"

if [[ ! -f "${MANIFEST}" ]]; then
    printf '错误：未找到运行时 "%s" 的适配 crate：%s\n' "${runtime}" "${MANIFEST}" >&2
    exit 1
fi

if [[ "${runtime}" == "glommio" ]]; then
    current_memlock="$(ulimit -l)"
    if [[ "${current_memlock}" != "unlimited" ]]; then
        # 将 memlock 解释为整数，缺省情况下 `ulimit -l` 返回 KB 单位。
        # 若数值低于 512 KiB，glommio 会在初始化 io_uring 时失败；提前给出指导性错误。
        if (( current_memlock < 512 )); then
            cat <<EOF >&2
错误：当前 memlock 限制仅为 ${current_memlock} KiB，无法满足 glommio 运行时需求（至少 512 KiB）。
请在宿主环境中提升 `ulimit -l` 后重试。
EOF
            exit 1
        fi
    fi
fi

pushd "${REPO_ROOT}" >/dev/null

echo "[runtime-tck] 使用运行时 ${runtime} 执行 spark-contract-tests TCK"

cargo test --manifest-path "${MANIFEST}"

popd >/dev/null
