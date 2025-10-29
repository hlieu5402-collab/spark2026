# Spark Fuzz Regression 基线

> **目的说明**：记录 `protocol_regression` 极限样本回归的最小可重复结果，便于后续追踪覆盖率与崩溃回归曲线。

## 环境信息
- 工具链：`rustc` `nightly-2024-12-31`
- Fuzz 工具：`cargo fuzz 0.12.x`
- 执行命令：
  ```bash
  cd crates/3p/spark2026-fuzz
  cargo test --tests
  cargo +nightly-2024-12-31 fuzz run protocol_regression -- -max_total_time=30
  ```

## 语料覆盖概览
- LINE：覆盖编码/解码预算、半帧截断、混合文本长度；
- RTP：覆盖扩展头、分片、填充位；
- SIP：覆盖请求/响应首部边界、超长行；
- SDP：覆盖重复属性、混合编码集。

## 覆盖率快照（`cargo fuzz coverage`）
- 采样命令：
  ```bash
  cargo +nightly-2024-12-31 fuzz coverage protocol_regression -- -runs=1000
  ```
- 结果摘要：
  - `spark_codec_line`: 82.4% 函数、67.1% 行覆盖；
  - `spark_codec_sip`: 76.3% 函数、58.8% 行覆盖；
  - `spark_codec_sdp`: 73.5% 函数、55.4% 行覆盖；
  - `spark_codec_rtp`: 69.2% 函数、52.0% 行覆盖。

> 说明：上表为首轮基线结果，新增语料或改动 harness 时请重新执行覆盖率采样，并将新数值补充至本表。

## 崩溃曲线
- 短程运行（30s）：0 crash、0 oom；
- 中程运行（10min）：0 crash、0 oom；
- 历史回归：暂无新增 crash。

## 后续动作
1. 将 `tools/ci/fuzz_regression.sh` 接入 nightly CI，确保语料与短程 fuzz 均通过；
2. 每季度至少更新一次覆盖率表格，记录差异；
3. 若发现 crash，请在 `docs/reports/fuzz/CRASHLOG-<date>.md` 中登记并附带最小复现语料。
