#!/usr/bin/env bash
# 教案级注释块: 说明脚本目标与使用方式.
# -----------------------------------------------------------------------------
# 教案级注释（符合 R1~R5 要求）
# 目标 / 架构定位（Why, R1.1-R1.3）
#   * 本脚本是 spark-core crate 的 CI 守门人，确保公共 API 不泄漏 tokio、async-std、smol、bytes、serde、
#     tracing 等第三方运行时或基础库符号，从而维持 crate “零第三方符号”红线，防止外部调用方对具体
#     实现产生依赖，保持可替换性。
#   * 脚本放置于 tools/ci，和其它守门脚本并列，在 CI 的 core job 中运行，属于整体质量防线中的 API
#     稳定性防护子模块。
#   * 使用的关键工具是 cargo public-api（扫描导出符号）与 ripgrep（过滤禁用前缀），组合构成“API
#     列表 -> 禁止前缀匹配”的管道型检测模式。
# 合同 / 输入输出（What, R3.1-R3.3）
#   * 输入：无额外参数，默认在仓库根目录执行；前置条件是已经安装 cargo、cargo public-api 与 rg，且
#     当前工作目录属于 spark2026 仓库（满足 git rev-parse）。
#   * 主要副作用：调用 cargo public-api -p spark-core 生成完整公共 API 列表。
#   * 输出：当检测通过时输出成功提示；若发现禁用前缀，则打印违规符号并以状态码 1 退出，向 CI 汇报
#     失败（后置条件是 CI 中断并提示整改）。
# 执行逻辑（How, R2.1-R2.2）
#   1. 解析仓库根目录并进入，保证命令相对路径一致。
#   2. 运行 cargo public-api 收集 spark-core 的公开符号；若命令失败，立即停止并把错误向上抛出。
#   3. 使用 rg 对输出执行正则匹配（包含 tokio::、async_std:: 等），若命中则打印违规列表并终止。
#   4. 若未命中，输出绿色提示并以 0 退出。
# 设计考量与边界（Trade-offs, R4.1-R4.3）
#   * 选择 rg 而非 shell 自带 grep，因为 rg 默认支持多模式 OR 且兼容 UTF-8；缺点是依赖额外工具，
#     但在 GitHub Actions 基础镜像中已预装，权衡后可接受。
#   * 为避免重复运行 cargo public-api 引发的性能负担，输出被一次性捕获并在内存中复用；若 API 列表
#     过大可能增大内存占用，不过当前 crate 规模可控。
#   * 错误路径会保留原始 cargo public-api 输出，以帮助开发者排查构建失败、缺失依赖等问题。
# 可维护性提示（Readability, R5.1-R5.2）
#   * 变量使用只读常量与函数封装，便于未来扩展更多禁用前缀或加入白名单逻辑。
#   * 如需豁免某个符号，应在 spark-core 内部通过 re-export 重命名或构建自定义类型，而不是修改此
#     守门脚本；否则会破坏“零第三方符号”承诺。
# -----------------------------------------------------------------------------

set -euo pipefail

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd)"
readonly TARGET_CRATE="spark-core"
readonly BANNED_PREFIX_PATTERN='tokio::|async_std::|smol::|bytes::|serde::|tracing::'

main() {
  cd "${REPO_ROOT}" >/dev/null

  local public_api_output
  if ! public_api_output=$(cargo public-api -p "${TARGET_CRATE}" 2>&1); then
    echo "[check_public_api_core] cargo public-api 执行失败，详细日志如下：" >&2
    echo "${public_api_output}" >&2
    return 1
  fi

  local banned_matches
  if banned_matches=$(printf '%s\n' "${public_api_output}" | rg -n -E "${BANNED_PREFIX_PATTERN}"); then
    echo "[check_public_api_core] 检测到以下违规第三方符号暴露在 spark-core 公共 API 中：" >&2
    echo "${banned_matches}" >&2
    echo "[check_public_api_core] 请移除上述符号或通过自定义封装避免直接暴露第三方前缀。" >&2
    return 1
  fi

  echo "[check_public_api_core] 检查通过：spark-core 公共 API 未暴露禁止的第三方前缀。"
}

main "$@"
