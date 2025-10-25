#!/usr/bin/env bash
# 教案级注释: 本脚本面向 CI 守门，明确禁止 spark-transport-* 对外暴露 Tokio 等运行时符号。
#
# 目标与架构背景 (Why):
# 1. 该脚本确保 spark-transport-* 相关 crate 的公共 API 中不会泄露 Tokio、async-std、smol、bytes 等运行时符号。
# 2. 脚本在 CI 链路的 tools/ci 下运行，通过 cargo-public-api 插件生成公共 API 清单并执行黑名单校验，
#    其作用类似“守门人”，避免下游依赖在 API 层面绑定具体的异步运行时实现。
# 3. 通过静态分析公共 API，而非运行时代码，可在不执行实际单元测试的前提下完成跨 crate 的接口审查。
#
# 处理流程 (How):
# 1. 通过 set -euo pipefail 保证任何步骤异常都会立即失败，防止 CI 忽略错误。
# 2. 利用 cargo metadata 枚举 workspace 中所有 spark-transport-* crate，逐个进行扫描，避免虚拟清单导致的失败。
# 3. 使用 cargo public-api (需预装 cargo-public-api 插件) 对单个 crate 收集公共 API，并优先尝试 --simplified --json 产出。
# 4. 若 --json 不可用则自动降级到纯文本输出，随后借助内嵌 Python 解析公共条目并匹配黑名单前缀。
# 5. 一旦检测到以 tokio::、async_std::、smol::、bytes:: 开头的符号，脚本会列出所有违规项并以非零状态退出。
#
# 输入/输出与契约 (What):
# - 输入：无显式参数，默认扫描当前工作目录下所有符合 spark-transport-* 的 crate。
# - 约束：调用者需在仓库根目录执行，且必须提前安装 cargo-public-api 插件；否则命令无法正确运行。
# - 返回：若未命中黑名单前缀，输出“所有公共 API 均未命中黑名单前缀”并返回 0；若存在违规，打印违规条目并返回 1。
#
# 前置条件与后置条件 (Contract):
# - 前置条件：1) 工作目录指向仓库根目录；2) cargo 命令可用；3) cargo-public-api 插件已安装。
# - 后置条件：1) 若返回 0，可认为公共 API 已通过黑名单检查；2) 若返回非 0，CI 会中断，需开发者整改。
#
# 设计权衡与注意事项 (Considerations):
# - 采用 Python 而非 jq，是为了在 CI 环境中不强制依赖额外的 shell 工具，确保跨平台一致性。
# - 若 cargo-public-api 缺少 --json 选项，会自动退回到解析纯文本输出，仍保证黑名单校验不中断。
# - 解析 JSON/纯文本时会尝试多个键 (public_item、item、path、item_path)，以兼容 cargo-public-api 版本间的字段变动。
# - 若 cargo public-api 命令失败（例如 crate 构建失败），脚本会立即退出，让 CI 显示真实错误。
#
# 边界情况提醒 (Risks):
# - 若后续新增黑名单前缀，可直接在 BLACKLIST_PREFIXES 数组中扩展即可。
# - 若 cargo-public-api 输出结构发生重大调整，应同步更新 Python 解析逻辑，否则可能漏检。
# - 若未来需要允许特定路径（如 tokio::runtime::Handle）作为例外，可在 Python 阶段增加白名单处理。
set -euo pipefail

BLACKLIST_PREFIXES=(
  "tokio::"
  "async_std::"
  "smol::"
  "bytes::"
)

readarray -t TRANSPORT_CRATES < <(
  cargo metadata --no-deps --format-version 1 |
    python3 -c 'import json,sys; metadata=json.load(sys.stdin); \
names=sorted({pkg["name"] for pkg in metadata.get("packages", []) if pkg["name"].startswith("spark-transport-")}); \
sys.stdout.write("\n".join(names) + ("\n" if names else ""))'
)

if ((${#TRANSPORT_CRATES[@]} == 0)); then
  echo "未找到 spark-transport-* crate，跳过公共 API 黑名单校验。"
  exit 0
fi

run_public_api() {
  local crate_name="$1"
  local output_file error_log
  output_file="$(mktemp)"
  error_log="$(mktemp)"
  if ! cargo public-api -p "${crate_name}" --simplified --json \
      > "${output_file}" 2> "${error_log}"; then
    if grep -q "unexpected argument '--json'" "${error_log}"; then
      echo "[${crate_name}] cargo-public-api 不支持 --json，回退到解析纯文本输出。" >&2
      if ! cargo public-api -p "${crate_name}" --simplified \
          > "${output_file}" 2>> "${error_log}"; then
        cat "${error_log}" >&2
        rm -f "${output_file}" "${error_log}"
        return 1
      fi
    else
      cat "${error_log}" >&2
      rm -f "${output_file}" "${error_log}"
      return 1
    fi
  fi

  local blacklist_payload
  blacklist_payload="$(printf '%s\n' "${BLACKLIST_PREFIXES[@]}")"
  PUBLIC_API_BLACKLIST="${blacklist_payload}" python3 - "${crate_name}" "${output_file}" <<'PY'
import json
import pathlib
import sys
import os

crate_name = sys.argv[1]
output_path = pathlib.Path(sys.argv[2])
blacklist_prefixes = [line for line in os.environ.get("PUBLIC_API_BLACKLIST", "").splitlines() if line]

raw_content = output_path.read_text(encoding="utf-8")

if raw_content.lstrip().startswith("["):
    data = json.loads(raw_content)
else:
    # 解析纯文本模式：每行一个符号声明，跳过空行。
    data = [line for line in raw_content.splitlines() if line.strip()]

def extract_candidate(entry):
    """提取可能代表符号路径的字段，兼容不同 cargo-public-api 版本。"""
    if isinstance(entry, str):
        return entry
    if not isinstance(entry, dict):
        return ""
    for key in ("public_item", "item", "path", "item_path", "name"):
        value = entry.get(key)
        if isinstance(value, str):
            return value
    return ""

violations = []
for raw_entry in data:
    candidate = extract_candidate(raw_entry)
    for prefix in blacklist_prefixes:
        if candidate.startswith(prefix):
            violations.append((prefix, candidate))
            break

if violations:
    print(f"[{crate_name}] 检测到以下公共 API 违反黑名单约束：")
    for prefix, candidate in violations:
        print(f"  - {candidate} (命中前缀: {prefix})")
    sys.exit(1)

PY
  rm -f "${output_file}" "${error_log}"
}

for crate in "${TRANSPORT_CRATES[@]}"; do
  echo "开始检查 crate: ${crate}"
  run_public_api "${crate}"
done

echo "所有 spark-transport-* crate 的公共 API 均未命中黑名单前缀。"
