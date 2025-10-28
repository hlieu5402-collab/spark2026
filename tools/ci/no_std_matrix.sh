#!/usr/bin/env bash
# 教案级注释块：no_std + alloc 编译矩阵验证脚本
# -----------------------------------------------------------------------------
# 1. 目标与架构定位（Why，R1.1-R1.3）
#    - 目标：验证指定 crate 在 `--no-default-features --features alloc` 组合下能够通过编译，
#      防止核心组件在嵌入式/零依赖场景退化。脚本输出构建日志，供 CI 上传为工件。
#    - 架构位置：作为 CI guardrail 的子任务，被主工作流在 `no-default-alloc` 特性集下调度。
#      它与 `feature_combos.sh` 输出的矩阵配合，覆盖 no_std 场景的最小可行集。
#    - 设计思想：采用“目标清单 + 逐包检查”模式，通过环境变量可扩展目标集合，默认
#      聚焦对无标准库要求最敏感的 crate（spark-core、spark-router、spark-transport）。
# 2. 契约定义（What，R3.1-R3.3）
#    - 输入：
#        * 环境变量 `NO_STD_MATRIX_PACKAGES`（空格分隔），默认值
#          `"spark-core spark-router spark-transport"`。
#        * 环境变量 `CI_ARTIFACT_DIR` 指定日志输出目录，默认 `ci-artifacts`。
#    - 输出：为每个包生成 `no-std-<pkg>.log`，记录 `cargo check` 过程。若任一包编译失败，
#      脚本立即以相同的退出码失败。
#    - 前置条件：
#        * 已安装 cargo，且可在仓库根目录执行。
#        * 目标 crate 在 `alloc` 特性下定义完整的依赖。
#    - 后置条件：成功时所有日志保留在 Artifact 目录；失败时同样保留日志以供排查。
# 3. 执行逻辑（How，R2.1-R2.2）
#    - 步骤：
#        a. 解析输入环境变量并创建日志目录。
#        b. 迭代目标包，调用 `cargo check -p <pkg> --no-default-features --features alloc`。
#        c. 将命令输出写入日志文件，同时在终端打印关键提示。
#        d. 如任一命令失败，保留日志并退出对应状态码。
#    - 实现技巧：使用 `tee` 同步输出，依赖 `set -o pipefail` 确保命令失败时脚本停止。
# 4. 设计权衡（R4.1-R4.3）
#    - 以 `cargo check` 代替 `cargo build`：在 CI 环境下可显著缩短时长，同时仍能验证
#      类型与依赖完整性；如需编译产物，可将 `CARGO_CMD` 调整为 `build`。
#    - 默认包集合针对核心业务路径，若未来扩展需评估 no_std 适配成本。
#    - 若存在互斥特性或运行时依赖，脚本会在日志中暴露具体错误，需结合 PR 评估是否
#      更新特性矩阵。
# 5. 可读性与维护（R5.1-R5.2）
#    - 日志统一前缀 `[no-std-matrix]`，并在成功/失败时打印摘要，方便快速定位问题。
#    - 函数化拆分逻辑，保留扩展点（例如 future TODO：支持多种特性组合）。
# -----------------------------------------------------------------------------

set -euo pipefail

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd)"
readonly ARTIFACT_DIR="${CI_ARTIFACT_DIR:-${REPO_ROOT}/ci-artifacts}"
readonly CARGO_CMD="${NO_STD_MATRIX_CARGO_CMD:-check}"

log() {
  printf '[no-std-matrix] %s\n' "$1" >&2
}

run_for_package() {
  local package="$1"
  local log_file="${ARTIFACT_DIR}/no-std-${package}.log"

  log "开始校验包 '${package}' 的 no_std(alloc) 组合。日志: ${log_file}"
  (
    cd "${REPO_ROOT}"
    cargo "${CARGO_CMD}" -p "${package}" --no-default-features --features alloc
  ) | tee "${log_file}"
}

main() {
  mkdir -p -- "${ARTIFACT_DIR}"
  local packages=( ${NO_STD_MATRIX_PACKAGES:-spark-core spark-router spark-transport} )

  local pkg
  for pkg in "${packages[@]}"; do
    if ! run_for_package "${pkg}"; then
      log "包 '${pkg}' 的 no_std 校验失败，详见日志。"
      exit 2
    fi
  done

  log "所有目标包已通过 no_std(alloc) 校验。"
}

main "$@"

