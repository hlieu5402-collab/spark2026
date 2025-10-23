#!/usr/bin/env bash
set -euo pipefail

# == Loom 模型测试驱动器（教案级注释） ==
#
# ## 意图 (Why)
# 1. 在 CI 中聚焦 `Cancellation`、`Budget`、`Channel` 三类原语的并发模型，
#    通过 Loom 穷举调度交错验证顺序性与死锁风险。
# 2. 封装 `RUSTFLAGS`、toolchain 与特性选择，确保脚本可在 GitHub Actions 与本地环境重用，
#    并保持“必过”要求。
#
# ## 所在位置与架构作用 (Where)
# - 位于 `tools/ci/` 目录，供持续集成与开发者本地预检复用；
# - 在整体验证链路中，与 `miri-test.sh` 搭配覆盖“内存可见性 + 顺序一致性”双重维度。
#
# ## 核心策略 (How)
# - 默认使用 `nightly-2025-06-15` toolchain；
# - 通过 `CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS` 注入 `--cfg spark_loom`，并设置
#   `CARGO_TARGET_APPLIES_TO_DEPENDENCIES=false`，确保仅对根包生效；
# - 启动命令：`cargo +<toolchain> test -p spark-core --features loom-model,std --test loom_concurrency`；
# - 允许通过环境变量自定义：
#   - `LOOM_FEATURES`：替换默认 features；
#   - `LOOM_NO_DEFAULT_FEATURES=1`：禁用默认特性；
#   - `LOOM_EXTRA_ARGS`：向 cargo 命令追加更多参数（如 `-- --nocapture`）。
#
# ## 契约 (What)
# - **输入**：仅读取环境变量，不接受位置参数；
# - **输出**：透传 cargo 测试日志；成功返回 0，失败返回非零码；
# - **前置条件**：Loom 已作为 dev-dependency 安装，且运行环境支持 `--cfg spark_loom`；
# - **后置条件**：取消/预算/通道三个场景的模型测试全部执行并通过。
#
# ## 风险提醒 (Gotchas)
# - 若需要扩大探索深度，可额外设置 `LOOM_MAX_PREEMPTIONS` 等环境变量，但运行时间会随之增加；
# - 当更换 nightly 版本时需同步更新脚本默认值以匹配缓存键；
# - 请勿在开启 Loom 时运行生产特性组合，以免误将模型配置带入正式构建。

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)
cd "$REPO_ROOT"

LOOM_TOOLCHAIN=${LOOM_TOOLCHAIN:-nightly-2025-06-15}
LOOM_FEATURES=${LOOM_FEATURES:-loom-model,std}
LOOM_NO_DEFAULT_FEATURES=${LOOM_NO_DEFAULT_FEATURES:-0}
LOOM_EXTRA_ARGS=${LOOM_EXTRA_ARGS:-}
LOOM_RUSTFLAGS=${LOOM_RUSTFLAGS:---cfg spark_loom}

ORIGINAL_TARGET_FLAGS=${CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS:-}
if [[ -n "${ORIGINAL_TARGET_FLAGS}" ]]; then
  export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="${ORIGINAL_TARGET_FLAGS} ${LOOM_RUSTFLAGS}"
else
  export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="${LOOM_RUSTFLAGS}"
fi
export CARGO_TARGET_APPLIES_TO_DEPENDENCIES=${CARGO_TARGET_APPLIES_TO_DEPENDENCIES:-false}
export LOOM_MAX_PREEMPTIONS=${LOOM_MAX_PREEMPTIONS:-2}

cmd=(cargo +"${LOOM_TOOLCHAIN}" test --package spark-core --test loom_concurrency)

if [[ -n "${LOOM_FEATURES}" ]]; then
  cmd+=(--features "${LOOM_FEATURES}")
fi

if [[ "${LOOM_NO_DEFAULT_FEATURES}" == "1" ]]; then
  cmd+=(--no-default-features)
fi

if [[ -n "${LOOM_EXTRA_ARGS}" ]]; then
  # shellcheck disable=SC2206
  extra=( ${LOOM_EXTRA_ARGS} )
  cmd+=("${extra[@]}")
fi

printf '::group::Running %s\n' "${cmd[*]}"
"${cmd[@]}"
printf '::endgroup::\n'
