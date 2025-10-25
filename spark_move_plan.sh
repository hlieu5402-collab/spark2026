#!/usr/bin/env bash
# ================================= 教案级注释 =================================
# 背景 (Why):
# 1. 当前仓库中的各个 `spark-*` crate 分布在仓库根目录和 `crates/` 目录下, 违反
#    约定的物理布局, 造成编译脚本与工具链难以按前缀批量发现模块。
# 2. 本脚本是一次性迁移脚本, 通过执行 `git mv` 将 crate 移动到约定路径, 以便
#    后续工具统一按 `crates/<域>/<crate>` 查找。
# 3. 迁移策略对标内部架构文档, 要求运行时 (runtime)、传输 (transport)、编解码
#    (codecs)、通用 (crates/spark-*) 以及三方依赖 (crates/3p) 采用分层目录结构。
#
# 契约 (What):
# - 输入: 在仓库根目录执行, 需要 git 工作区干净 (或开发者已知并接受文件的
#   `git mv` 覆盖行为)。
# - 前置条件: 当前目录必须包含 `Cargo.toml`、`spark-codec-rtp/` 等 crate 的旧路径。
# - 后置条件: 目标 crate 将位于 `crates/<domain>/...`, 原位置不再保留, git 历史
#   通过 `git mv` 保持迁移关系。
#
# 设计权衡与注意事项 (Trade-offs & Gotchas):
# - 使用 `git mv` 而不是 `mv`, 以保留历史并自动记录删除/新增操作。
# - 迁移 `spark-core/` 时目的路径 `crates/spark-core` 已存在占位符, 需强制覆盖。
#   脚本通过 `git mv -f` 确保替换, 但也意味着若本地修改未提交, 会被覆盖。
# - 如需新增目录 (例如 `crates/codecs`), 必须在移动前调用 `mkdir -p`。
# - 本脚本幂等性有限: 再次执行将因源目录缺失而失败, 因此只应用于一次迁移。
#
# 执行步骤 (How):
# 1. 验证当前目录存在 `Cargo.toml`, 以确认在仓库根目录运行。
# 2. 针对 `MOVE_PLAN` 中的每个键值对, 创建目标目录、调用 `git mv -f` 迁移。
# 3. 遇到缺少源目录时立即失败, 防止部分迁移导致仓库不一致。
# ==============================================================================
set -euo pipefail

# 第 1 步: 校验执行位置, 避免在错误目录运行导致大规模误删。
if [[ ! -f Cargo.toml ]]; then
  echo "[错误] 请在仓库根目录执行本脚本 (缺少 Cargo.toml)." >&2
  exit 1
fi

# 第 2 步: 定义迁移映射表, 键为旧路径, 值为新路径。
# 说明:
# - 运行时相关 crate 目前仅有 `spark-core`, 暂定位于 `crates/spark-core` 层。
# - 传输层包含 `spark-transport-*` 系列, 统一归档到 `crates/transport/`。
# - 编解码器放入 `crates/codecs/`, 便于集中管理。
declare -A MOVE_PLAN=(
  ["spark-codec-line"]="crates/codecs/spark-codec-line"
  ["spark-codec-rtcp"]="crates/codecs/spark-codec-rtcp"
  ["spark-codec-rtp"]="crates/codecs/spark-codec-rtp"
  ["spark-codec-sdp"]="crates/codecs/spark-codec-sdp"
  ["spark-codec-sip"]="crates/codecs/spark-codec-sip"
  ["spark-core"]="crates/spark-core"
  ["crates/spark-transport-tcp"]="crates/transport/spark-transport-tcp"
  ["crates/spark-transport-udp"]="crates/transport/spark-transport-udp"
  ["crates/spark-transport-tls"]="crates/transport/spark-transport-tls"
  ["crates/spark-transport-quic"]="crates/transport/spark-transport-quic"
)

# 第 3 步: 执行迁移。
for SRC in "${!MOVE_PLAN[@]}"; do
  DEST="${MOVE_PLAN[${SRC}]}"
  if [[ ! -d "${SRC}" ]]; then
    echo "[错误] 源目录不存在: ${SRC}." >&2
    exit 1
  fi
  mkdir -p "$(dirname "${DEST}")"
  echo "[INFO] git mv -f ${SRC} ${DEST}"
  git mv -f "${SRC}" "${DEST}"
done

echo "[INFO] 迁移完成, 请手动更新 Cargo.toml 等配置中的路径。"
