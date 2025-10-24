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

# 教案级注释：新增的 API 基线文件路径与派生输出目录。
# -----------------------------------------------------------------------------
# Why (R1):
#   * BASELINE_FILE 指向事先固化的 JSON 基线，用于比较“默认特性”场景下的公共 API。
#     通过静态文件，我们可以识别新增导出，并针对增量做禁用前缀审查，避免每次全量 diff。
#   * PUBLIC_API_LOG_DIR 是脚本执行期间存放临时输出的目录，保证默认特性与
#     --no-default-features 场景彼此隔离，便于失败时定位问题。
# How (R2):
#   * mktemp -d 创建唯一目录，防止多个并行 CI job 互相覆盖；成功后 cleanup 负责释放。
# What (R3):
#   * BASELINE_FILE: JSON 文件，内容为 cargo public-api --simplified 输出的字符串数组。
#   * PUBLIC_API_LOG_DIR: 目录路径，内部存放各变体的原始文本输出与调试资料。
# Trade-offs (R4):
#   * 使用 JSON 作为基线格式牺牲了人眼 diff 友好度，但换取了脚本计算新符号时的易解析性；
#     如需直接阅读，可配合 jq 或转换工具。
#   * 采用目录而非单个文件会多占几个 inode，但能存放多个变体的产物，提升扩展性。
# Readability (R5):
#   * 变量名与注释均采用“baseline/log_dir”直观命名，后续维护者无需额外查阅即可理解用途。
# -----------------------------------------------------------------------------
readonly BASELINE_FILE="${REPO_ROOT}/tools/baselines/spark-core.public-api.json"
readonly PUBLIC_API_LOG_DIR="$(mktemp -d)"

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

# 教案级注释：禁用前缀逗号串用于向 Python 子进程传递策略。
# -----------------------------------------------------------------------------
# Why (R1):
#   * Python 子进程通过环境变量读取禁用前缀列表；在此先行拼接可避免跨语言解析数组的复杂度。
# How (R2):
#   * 通过 IFS=',' 将 Bash 数组展开为逗号连接的单行字符串，再传入 CHECK_PUBLIC_API_BANNED。
# What (R3):
#   * 字符串格式示例："tokio::,async_std::,..."，供 Python 按逗号拆分。
# Trade-offs (R4):
#   * 采用环境变量传递避免额外临时文件，虽然牺牲了少量注释篇幅。
# Readability (R5):
#   * 命名 BANNED_PREFIXES_JOINED 与 describe_banned_prefixes 保持一致，利于维护者快速检索。
# -----------------------------------------------------------------------------
readonly BANNED_PREFIXES_JOINED="$(IFS=','; echo "${BANNED_PREFIXES[*]}")"

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

# 教案级注释：清理逻辑与输出保留策略的升级说明。
# -----------------------------------------------------------------------------
# Why (R1):
#   * cleanup 在脚本结束时统一管理 mktemp 目录，避免 CI 工作区残留大量日志；
#     同时在失败场景下提示开发者保留路径，用于复盘具体变体的输出。
# How (R2):
#   * 通过 trap 捕获 EXIT，将退出码传入 cleanup；成功时递归删除目录，失败时仅打印提示。
# What (R3):
#   * 参数 $1：退出码；前置条件是 PUBLIC_API_LOG_DIR 已成功创建；后置条件为：
#       - exit_code == 0：目录被删除；
#       - exit_code != 0：目录保留并输出提示语，便于调试。
# Trade-offs (R4):
#   * 成功路径多了一次 rm -rf，但换取了执行完毕后干净的工作区；
#   * 失败路径要求开发者手动清理目录，但能提供更多上下文。
# Readability (R5):
#   * 通过 printf 格式化输出，告知目录路径并指向具体文件，便于后续脚本扩展。
# -----------------------------------------------------------------------------
cleanup() {
  local exit_code="$1"

  if [[ "${exit_code}" -eq 0 ]]; then
    rm -rf "${PUBLIC_API_LOG_DIR}"
    return
  fi

  echo "[check_public_api_core] 公共 API 调试输出位于 ${PUBLIC_API_LOG_DIR}，请根据需要手动清理。" >&2
}

trap 'cleanup $?' EXIT

# 教案级注释：确认基线文件存在，以免脚本在缺失配置时误判。
# -----------------------------------------------------------------------------
# Why (R1):
#   * ensure_baseline_file 在执行 diff 前提前验证文件是否存在，避免 Python 脚本抛出难以理解的 I/O 异常。
# How (R2):
#   * 使用 test -f 判断文件存在性；若不存在则输出指引信息（提示运行基线生成命令）。
# What (R3):
#   * 无入参，读取全局 BASELINE_FILE；成功返回 0，失败直接 exit 1。
# Trade-offs (R4):
#   * 增加一次磁盘检查，可忽略；换取的是对配置缺失的显式报错。
# -----------------------------------------------------------------------------
ensure_baseline_file() {
  if [[ ! -f "${BASELINE_FILE}" ]]; then
    echo "[check_public_api_core] 未找到基线文件：${BASELINE_FILE}" >&2
    echo "[check_public_api_core] 请先执行 cargo public-api 并同步 tools/baselines/spark-core.public-api.json 后再运行本脚本。" >&2
    exit 1
  fi
}

# 教案级注释：抽象执行 cargo public-api 的公共函数，复用在不同特性组合中。
# -----------------------------------------------------------------------------
# Why (R1):
#   * run_public_api_variant 将重复的命令行拼装与错误处理集中在一处，确保默认特性与
#     --no-default-features 场景遵循一致的日志策略。
# How (R2):
#   * 接收变体 key（用于输出文件命名）与人类可读描述，并接受额外 cargo public-api 参数；
#     执行命令时强制添加 --simplified 与 --color never，输出写入对应的文本文件。
#   * 若命令失败，立即打印日志内容并返回 1，交由上层处理。
# What (R3):
#   * 参数：variant_key、variant_label、其余为可选的 cargo public-api CLI 参数；
#   * 返回值：在 stdout 打印生成的文本文件路径，供调用方 capture；
#   * 前置条件：依赖工具已安装、工作目录在仓库根目录。
# Trade-offs (R4):
#   * 每个变体都创建独立文件，略微增加 I/O，但方便并行扩展更多特性组合。
# -----------------------------------------------------------------------------
run_public_api_variant() {
  local variant_key="$1"
  local variant_label="$2"
  shift 2
  local stdout_file="${PUBLIC_API_LOG_DIR}/${variant_key}.txt"
  local stderr_file="${PUBLIC_API_LOG_DIR}/${variant_key}.stderr.txt"
  local extra_args=()

  if [[ "$#" -gt 0 ]]; then
    extra_args=("$@")
  fi

  if ! cargo public-api -p "${TARGET_CRATE}" --simplified --color never "${extra_args[@]}" \
    >"${stdout_file}" 2>"${stderr_file}"; then
    echo "[check_public_api_core] ${variant_label} 执行 cargo public-api 失败，stderr 日志如下：" >&2
    cat "${stderr_file}" >&2
    echo "[check_public_api_core] 如需查看 stdout，请检查 ${stdout_file}。" >&2
    return 1
  fi

  if [[ -s "${stderr_file}" ]]; then
    echo "[check_public_api_core] ${variant_label} 运行产生的 stderr 已保存到 ${stderr_file}（可能包含编译告警）。" >&2
  fi

  printf '%s\n' "${stdout_file}"
}

# 教案级注释：对比默认特性输出与基线，聚焦新增符号的禁用前缀审查。
# -----------------------------------------------------------------------------
# Why (R1):
#   * check_new_symbols_against_baseline 只审查“新增”的公共 API，避免历史遗留问题造成噪音；
#     若新符号包含禁用前缀，立即阻止合并。
# How (R2):
#   * 调用 Python 读取 baseline JSON 与当前输出文本，计算差集并筛选包含禁用前缀的条目；
#     同时输出新增符号列表供开发者确认。
# What (R3):
#   * 入参：variant_label（用于日志）、output_file（当前文本文件路径）。
#   * 依赖：环境变量 BANNED_PREFIXES 提供逗号分隔的禁用前缀。
#   * 返回：0 表示新增符号安全；若发现违规则打印列表并返回 1。
# Trade-offs (R4):
#   * 使用 Python 增加了运行时依赖，但换取了更清晰的集合操作语义与 UTF-8 处理能力。
# -----------------------------------------------------------------------------
check_new_symbols_against_baseline() {
  local variant_label="$1"
  local output_file="$2"

  CHECK_PUBLIC_API_BANNED="${BANNED_PREFIXES_JOINED}" python3 - <<'PY' "${variant_label}" "${BASELINE_FILE}" "${output_file}"
import json
import os
import sys
from pathlib import Path

variant_label, baseline_path, output_path = sys.argv[1:4]
banned_prefixes = [p for p in os.environ.get("CHECK_PUBLIC_API_BANNED", "").split(",") if p]

baseline = json.loads(Path(baseline_path).read_text(encoding="utf-8"))
current = Path(output_path).read_text(encoding="utf-8").splitlines()

baseline_set = set(baseline)
new_symbols = [line for line in current if line not in baseline_set]

if new_symbols:
    print(f"[check_public_api_core] {variant_label} 新增公共 API {len(new_symbols)} 项：")
    for item in new_symbols:
        print(f"  {item}")
else:
    print(f"[check_public_api_core] {variant_label} 未检测到新增公共 API。")

violations = [item for item in new_symbols if any(prefix in item for prefix in banned_prefixes)]

if violations:
    print("[check_public_api_core] 以下新增符号包含禁用前缀，已拒绝本次提交：", file=sys.stderr)
    for item in violations:
        print(f"  {item}", file=sys.stderr)
    print(f"[check_public_api_core] 禁止暴露的前缀列表：{', '.join(banned_prefixes)}", file=sys.stderr)
    raise SystemExit(1)
PY
}

# 教案级注释：对整份公共 API 文本执行兜底扫描，防止漏网之鱼。
# -----------------------------------------------------------------------------
# Why (R1):
#   * 全量扫描确保即便基线未覆盖的场景（如 --no-default-features）也不会引入禁用前缀。
# How (R2):
#   * 借助 ripgrep 的正则匹配能力，直接在输出文本中查找任何禁用前缀。
# What (R3):
#   * 入参：variant_label（日志用途）、output_file（文本文件路径）。
#   * 返回：0 表示未命中；若命中则打印违规行与文件路径并返回 1。
# Trade-offs (R4):
#   * rg 运行两次（默认 / no-default）会增加数百毫秒，但换来兜底可靠性。
# -----------------------------------------------------------------------------
full_scan_for_banned_prefixes() {
  local variant_label="$1"
  local output_file="$2"
  local banned_matches

  if banned_matches=$(rg -n "${BANNED_PREFIX_PATTERN}" "${output_file}"); then
    echo "[check_public_api_core] ${variant_label} 全量扫描命中禁用前缀：" >&2
    echo "${banned_matches}" >&2
    echo "[check_public_api_core] 禁止暴露的前缀列表：$(describe_banned_prefixes)。" >&2
    echo "[check_public_api_core] 相关输出位于 ${output_file}，请修复后重试。" >&2
    return 1
  fi

  echo "[check_public_api_core] ${variant_label} 全量扫描通过。"
}

main() {
  cd "${REPO_ROOT}" >/dev/null

  # 教案级注释：在执行核心逻辑前确保依赖工具与基线配置齐备。
  # ---------------------------------------------------------------------------
  ensure_tool cargo
  ensure_tool cargo-public-api
  ensure_tool rg
  ensure_tool python3
  ensure_baseline_file

  local default_output
  default_output="$(run_public_api_variant "default" "默认特性场景")" || return 1
  check_new_symbols_against_baseline "默认特性场景" "${default_output}" || return 1
  full_scan_for_banned_prefixes "默认特性场景" "${default_output}" || return 1

  local no_default_output
  if ! no_default_output="$(run_public_api_variant "no-default" "--no-default-features 场景" --no-default-features --features alloc)"; then
    echo "[check_public_api_core] --no-default-features 场景构建失败，尝试启用 std 回退以完成公共 API 扫描。" >&2
    no_default_output="$(run_public_api_variant "no-default-fallback" "--no-default-features+std 回退场景" --no-default-features --features alloc,std)" || return 1
  fi
  full_scan_for_banned_prefixes "--no-default-features 检查" "${no_default_output}" || return 1

  echo "[check_public_api_core] 全部检查完成，spark-core 公共 API 保持零第三方前缀。"
}

main "$@"

