#!/usr/bin/env bash
#
# 教案级说明：
# - 目标（Why）：确保 `DEFAULT_OBSERVABILITY_CONTRACT` 的键名同时出现在文档中，避免指标/日志/追踪键漂移，守住跨团队协同的统一契约。
# - 角色定位：作为 CI 守门脚本，在 `make ci-lints` 及相关流水线中执行，提前阻断文档与实现不一致的风险。
# - 设计思路（How）：
#   1. 使用 `git rev-parse` 锁定仓库根目录，避免相对路径歧义。
#   2. 通过 Python 脚本解析 `contract.rs` 中 `DEFAULT_OBSERVABILITY_CONTRACT` 的四组字符串数组。
#   3. 再解析 `docs/observability-contract.md` 内 JSON 清单，保证键集合与顺序完全一致。
#   4. 在发现缺失、冗余或顺序差异时给出精确 diff，便于开发者即时修复。
# - 契约（What）：
#   * 输入：无显式参数；依赖仓库中既定文件路径。
#   * 前置条件：`spark-core/src/contract.rs` 与 `docs/observability-contract.md` 按约定维护。
#   * 后置条件：若校验通过输出成功提示；否则非零退出并指出差异。
# - 设计考量（Trade-offs）：
#   * 采用内联 Python 是为了获得稳健的字符串解析能力，同时保持脚本入口简单。
#   * 校验顺序而非仅集合，可捕获文档重排造成的认知偏差，付出的成本仅为一次列表比较。
#   * 如果未来 contract 定义迁移，需同步更新 `CONTRACT_FILE` 与正则；脚本在失败信息中已有指引。
#
set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
CONTRACT_FILE="${REPO_ROOT}/spark-core/src/contract.rs"
DOC_FILE="${REPO_ROOT}/docs/observability-contract.md"

python3 - <<'PY' "${CONTRACT_FILE}" "${DOC_FILE}"
import json
import pathlib
import re
import sys
from typing import Dict, Iterable, List

contract_path = pathlib.Path(sys.argv[1])
doc_path = pathlib.Path(sys.argv[2])

ARRAY_PATTERN = re.compile(r"&\[(?P<body>.*?)\],", re.DOTALL)
JSON_BLOCK_PATTERN = re.compile(r"```json\s*(\{.*?\})\s*```", re.DOTALL)


def extract_arrays(text: str) -> List[List[str]]:
    """从常量定义中抽取四组字符串数组。"""
    try:
        start = text.index("DEFAULT_OBSERVABILITY_CONTRACT")
    except ValueError as exc:  # pragma: no cover - 结构被改动时提醒
        raise ValueError(
            "未找到 DEFAULT_OBSERVABILITY_CONTRACT，请检查源码结构是否调整。"
        ) from exc

    suffix = text[start:]
    matches = ARRAY_PATTERN.findall(suffix)
    if len(matches) < 4:
        raise ValueError(
            "未能解析出 4 组数组，请确认 ObservabilityContract::new 的参数结构未被改写。"
        )

    arrays: List[List[str]] = []
    for body in matches[:4]:
        items = re.findall(r'"([^"\\]*(?:\\.[^"\\]*)*)"', body)
        arrays.append([bytes(item, "utf-8").decode("unicode_escape") for item in items])
    return arrays


def find_duplicates(items: Iterable[str]) -> List[str]:
    """返回列表中出现重复的元素集合，保持发现顺序。"""
    seen = set()
    duplicates: List[str] = []
    for item in items:
        if item in seen and item not in duplicates:
            duplicates.append(item)
        else:
            seen.add(item)
    return duplicates


try:
    contract_arrays = extract_arrays(contract_path.read_text(encoding="utf-8"))
except ValueError as exc:
    print(f"[ERROR] {exc}", file=sys.stderr)
    sys.exit(1)

contract_data: Dict[str, List[str]] = {
    "metric_names": contract_arrays[0],
    "log_fields": contract_arrays[1],
    "trace_keys": contract_arrays[2],
    "audit_schema": contract_arrays[3],
}

contract_dups = {
    key: find_duplicates(values) for key, values in contract_data.items() if find_duplicates(values)
}
if contract_dups:
    for key, duplicates in contract_dups.items():
        print(
            f"[ERROR] 源码中的 {key} 含有重复键：{duplicates}。请先去重后再执行校验。",
            file=sys.stderr,
        )
    sys.exit(1)

doc_text = doc_path.read_text(encoding="utf-8")
json_match = JSON_BLOCK_PATTERN.search(doc_text)
if not json_match:
    print(
        f"[ERROR] 未能在 {doc_path} 中找到 JSON 契约清单，请确保文档包含与 CI 对齐的代码块。",
        file=sys.stderr,
    )
    sys.exit(1)

try:
    doc_data = json.loads(json_match.group(1))
except json.JSONDecodeError as exc:
    print(
        f"[ERROR] 解析 {doc_path} 中 JSON 契约清单失败：{exc}",
        file=sys.stderr,
    )
    sys.exit(1)

exit_code = 0

for key, expected in contract_data.items():
    documented = doc_data.get(key)
    if documented is None:
        print(f"[ERROR] 文档缺少字段 {key!r}。", file=sys.stderr)
        exit_code = 1
        continue

    if not isinstance(documented, list):
        print(
            f"[ERROR] 文档中的 {key} 应为数组，但实际类型为 {type(documented).__name__}。",
            file=sys.stderr,
        )
        exit_code = 1
        continue

    for value in documented:
        if not isinstance(value, str):
            print(
                f"[ERROR] 文档中的 {key} 包含非字符串元素：{value!r}。",
                file=sys.stderr,
            )
            exit_code = 1
            break
    else:
        duplicates = find_duplicates(documented)
        if duplicates:
            print(
                f"[ERROR] 文档中的 {key} 存在重复项：{duplicates}。",
                file=sys.stderr,
            )
            exit_code = 1

    if exit_code:
        continue

    if expected != documented:
        missing = [item for item in expected if item not in documented]
        extra = [item for item in documented if item not in expected]

        if missing:
            print(f"[ERROR] 文档缺少 {key} 条目：{missing}", file=sys.stderr)
        if extra:
            print(f"[ERROR] 文档存在额外的 {key} 条目：{extra}", file=sys.stderr)

        if not missing and not extra and len(expected) == len(documented):
            mismatch = [
                (idx, exp, documented[idx])
                for idx, exp in enumerate(expected)
                if documented[idx] != exp
            ]
            if mismatch:
                formatted = ", ".join(
                    f"索引 {idx}: 源码={exp!r}, 文档={doc!r}" for idx, exp, doc in mismatch
                )
                print(
                    f"[ERROR] 文档中的 {key} 顺序与源码不一致：{formatted}",
                    file=sys.stderr,
                )

        exit_code = 1

if exit_code == 0:
    print("[OK] DEFAULT_OBSERVABILITY_CONTRACT 键名已通过文档一致性校验。")

sys.exit(exit_code)
PY
