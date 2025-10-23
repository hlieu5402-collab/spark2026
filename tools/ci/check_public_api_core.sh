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
#   * 使用的关键工具是 cargo public-api（扫描导出符号）与 ripgrep（过滤禁用前缀），通过“生成 API
#     日志文件 -> 文件内执行禁用前缀匹配”的两阶段检测模式。
# 合同 / 输入输出（What, R3.1-R3.3）
#   * 输入：无额外参数，默认在仓库根目录执行；前置条件是已经安装 cargo、cargo public-api 与 rg，且
#     当前工作目录属于 spark2026 仓库（满足 git rev-parse）。
#   * 主要副作用：调用 cargo public-api -p spark-core 生成完整公共 API 列表。
#   * 输出：当检测通过时输出成功提示；若发现禁用前缀，则打印违规符号并以状态码 1 退出，向 CI 汇报
#     失败（后置条件是 CI 中断并提示整改）。
# 执行逻辑（How, R2.1-R2.2）
#   1. 解析仓库根目录并进入，保证命令相对路径一致。
#   2. 运行 cargo public-api，并通过 tee 同步写入临时日志文件；若命令失败，立即停止并打印日志内容。
#   3. 使用 rg 针对日志文件执行正则匹配（包含 tokio::、async_std:: 等），若命中则打印违规列表并终止。
#   4. 若未命中，输出绿色提示并以 0 退出。
# 设计考量与边界（Trade-offs, R4.1-R4.3）
#   * 选择 rg 而非 shell 自带 grep，因为 rg 默认支持多模式 OR 且兼容 UTF-8；缺点是依赖额外工具，
#     但在 GitHub Actions 基础镜像中已预装，权衡后可接受。
#   * 为避免重复运行 cargo public-api 引发的性能负担，输出写入临时文件并复用；磁盘 I/O 带来轻微开销
#     但换取了失败后可复现的上下文记录。
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

# 教案级注释：禁用前缀数组的存在意义与架构作用。
# -----------------------------------------------------------------------------
# Why (R1):
#   * 通过集中维护 BANNED_PREFIXES 数组，我们将“禁止暴露的第三方运行时符号”抽象为一组可配置的数据，
#     避免在正则中硬编码，便于未来新增或删除前缀时保持脚本可读性。
#   * 在整体 CI 架构中，该数组是“策略层”，负责描述业务规则；后续函数负责将策略转换为具体执行指令。
# How (R2):
#   * 数组内的每个元素都是 cargo public-api 输出中可能出现的字符串前缀；稍后会由 build_banned_regex
#     组合为正则，用于 ripgrep 匹配。
# What (R3):
#   * 数组元素是纯字符串（不含特殊正则字符），调用者不需要做额外转义。
# Trade-offs (R4):
#   * 使用数组而非直接写正则，牺牲了几行脚本长度，换取了策略层的显式性；
#   * 若未来禁用列表过长，可考虑将其迁移到外部 JSON/YAML 配置，此处仍保持简单实现。
# Readability (R5):
#   * 使用逐行排列使得 diff 清晰，便于 code review。
# -----------------------------------------------------------------------------
readonly BANNED_PREFIXES=(
  "tokio::"
  "async_std::"
  "smol::"
  "bytes::"
  "serde::"
  "tracing::"
)

# 教案级注释：将策略层数组转译为正则表达式的函数。
# -----------------------------------------------------------------------------
# Why (R1):
#   * build_banned_regex 是策略与执行之间的粘合剂，将多个前缀拼接为 ripgrep 所需的 "a|b|c" 结构。
#   * 在 CI 体系中，该函数属于“适配层”，确保我们可以在 shell 环境下灵活拼接模式。
# How (R2):
#   * 依次遍历 BANNED_PREFIXES，通过累加字符串的方式构建模式；为了避免管道前导 |，使用分隔符变量。
#   * 伪代码：regex = join('|', BANNED_PREFIXES)。
# What (R3):
#   * 无入参，依赖全局只读 BANNED_PREFIXES；返回值是单行字符串（正则模式）。
#   * 前置条件：BANNED_PREFIXES 已定义；后置条件：返回的字符串可安全传入 rg -E。
# Trade-offs (R4):
#   * 选择纯 bash 累加而非 printf IFS 拼接，是为了避免旧版本 bash 对数组展开的兼容性问题。
#   * 若未来引入特殊字符，需要额外转义逻辑，目前前缀均为字母与冒号，无须处理。
# Readability (R5):
#   * 使用清晰的变量命名（regex、separator），并在循环内注释关键步骤，便于维护者理解。
# -----------------------------------------------------------------------------
build_banned_regex() {
  local regex=""
  local separator=""
  local prefix

  for prefix in "${BANNED_PREFIXES[@]}"; do
    regex+="${separator}${prefix}"
    separator='|'
  done

  printf '%s' "${regex}"
}

readonly BANNED_PREFIX_PATTERN="$(build_banned_regex)"

# 教案级注释：人类可读的禁用前缀列表文本，用于错误信息。
# -----------------------------------------------------------------------------
# Why (R1):
#   * describe_banned_prefixes 面向开发者输出友好提示，帮助他们理解违反规则的具体列表。
# How (R2):
#   * 与正则拼接类似，但分隔符使用 ", "，可读性更强。
# What (R3):
#   * 返回字符串，供日志打印；无副作用。
# Trade-offs (R4):
#   * 单独函数避免多处重复拼接逻辑，也便于未来国际化。
# -----------------------------------------------------------------------------
describe_banned_prefixes() {
  local description=""
  local separator=""
  local prefix

  for prefix in "${BANNED_PREFIXES[@]}"; do
    description+="${separator}${prefix}"
    separator=", "
  done

  printf '%s' "${description}"
}

# 教案级注释：工具依赖检查函数确保脚本运行环境满足前置条件。
# -----------------------------------------------------------------------------
# Why (R1):
#   * ensure_tool 将“脚本假定外部依赖已安装”的隐式约束显式化，避免 CI 因工具缺失而出现难以理解的错误。
# How (R2):
#   * command -v 检查可执行文件路径，失败时输出指导语并终止。
# What (R3):
#   * 入参 tool_name（字符串）：需存在于 PATH 的命令。
#   * 返回：成功时无输出，失败时退出 1；前置条件是 PATH 正确设置。
#   * 后置条件：调用者可以安全执行对应命令。
# Trade-offs (R4):
#   * 每个工具做一次检查，增加了毫秒级开销，但换取了更可解释的错误信息。
# -----------------------------------------------------------------------------
ensure_tool() {
  local tool_name="$1"

  if ! command -v "${tool_name}" >/dev/null 2>&1; then
    echo "[check_public_api_core] 缺少依赖工具：${tool_name}。请在本地安装后重试。" >&2
    exit 1
  fi
}

# 教案级注释补充: 说明临时文件策略与故障排查路径。
# -----------------------------------------------------------------------------
#   * Why (R1): 通过缓存 cargo public-api 的原始输出，开发者在 CI 失败时可以从生成的日志文件
#     中定位违规符号以外的上下文（例如 API 所在模块、签名等），避免重复运行昂贵命令。
#   * How (R2): mktemp 创建临时文件，将 public-api 输出 tee 到文件并交由后续 rg 分析；脚本成功
#     时自动删除临时文件，失败时保留以便复盘。
#   * What (R3): PUBLIC_API_LOG_FILE 是只读路径，生命周期受 trap 控制；违反禁令时输出违规行
#     号并引用该文件。
#   * Trade-offs (R4): 相比直接管道，增加一次磁盘写入换取调试便利。由于输出规模较小且运行频率
#     低，该成本可接受；若未来输出超大可改为压缩或限制写入字段。
# -----------------------------------------------------------------------------
readonly PUBLIC_API_LOG_FILE="$(mktemp)"

cleanup() {
  # R1-R5: 解释 cleanup 在架构中的角色及调用契约。
  #   * Why: CI 结束后清理临时文件，避免污染工作目录；当脚本失败时保留文件以供开发者调试。
  #   * How: trap 捕获 EXIT，并把最终退出码传入；成功时 rm -f，失败时打印提示保留路径。
  #   * What: 参数 $1 为退出码；前置条件是 PUBLIC_API_LOG_FILE 已创建；后置条件：
  #           - 成功（0）时文件被删除；
  #           - 失败时文件保留，同时在标准错误输出路径提示。
  local exit_code="$1"
  if [[ "${exit_code}" -eq 0 ]]; then
    rm -f "${PUBLIC_API_LOG_FILE}"
    return
  fi

  echo "[check_public_api_core] 公共 API 列表保留在 ${PUBLIC_API_LOG_FILE} 以供调试。" >&2
}

trap 'cleanup $?' EXIT

main() {
  cd "${REPO_ROOT}" >/dev/null

  # 教案级注释：在执行核心逻辑前确保依赖工具可用。
  # ---------------------------------------------------------------------------
  # Why (R1):
  #   * main 函数在进入 cargo public-api 之前显式验证依赖，避免执行中途才失败。
  # How (R2):
  #   * 依次调用 ensure_tool 检查 cargo、cargo-public-api（二进制名）以及 rg。
  # What (R3):
  #   * 所有 ensure_tool 调用均无返回值；若任一失败，脚本立即退出，后续逻辑不再执行。
  # Trade-offs (R4):
  #   * 增加三次 PATH 查找，但换取失败时的直接提示，降低排障成本。
  # ---------------------------------------------------------------------------
  ensure_tool cargo
  ensure_tool cargo-public-api
  ensure_tool rg

  # R2: 以 tee 将 cargo public-api 输出同时写入日志与后续 rg；stdout/stderr 合并到日志，便于分析。
  if ! cargo public-api -p "${TARGET_CRATE}" 2>&1 | tee "${PUBLIC_API_LOG_FILE}" >/dev/null; then
    echo "[check_public_api_core] cargo public-api 执行失败，详细日志如下：" >&2
    cat "${PUBLIC_API_LOG_FILE}" >&2
    return 1
  fi

  local banned_matches
  if banned_matches=$(rg -n -E "${BANNED_PREFIX_PATTERN}" "${PUBLIC_API_LOG_FILE}"); then
    echo "[check_public_api_core] 检测到以下违规第三方符号暴露在 spark-core 公共 API 中：" >&2
    echo "${banned_matches}" >&2
    echo "[check_public_api_core] 禁止暴露的前缀列表：$(describe_banned_prefixes)。" >&2
    echo "[check_public_api_core] 完整 API 列表已保存在 ${PUBLIC_API_LOG_FILE}，用于进一步排查。" >&2
    echo "[check_public_api_core] 请移除上述符号或通过自定义封装避免直接暴露第三方前缀。" >&2
    return 1
  fi

  echo "[check_public_api_core] 检查通过：spark-core 公共 API 未暴露禁止的第三方前缀。"
}

main "$@"
