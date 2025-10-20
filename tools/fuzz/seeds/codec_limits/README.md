# codec_limits 种子说明

本目录使用 `hex:` 前缀的 ASCII 文本保存最小触发样例。运行 `cargo fuzz` 时，模糊器会将这些文本输入传递给目标；
`codec_limits` 模糊目标中的 `decode_hex_seed` 会在运行期解析十六进制字符串并还原出真实字节序列。

- `oversized_frame.seed`：触发帧长超出上限的拒绝分支。
- `depth_overflow.seed`：构造递归深度超过限制的输入，验证栈深守卫。
- `budget_exhaustion.seed`：消耗预算超过阈值，触发预算耗尽路径。

> 注意：若新增种子，请保持 ASCII 可读格式，以便仓库在 `no-binary` 限制下正常存储。
