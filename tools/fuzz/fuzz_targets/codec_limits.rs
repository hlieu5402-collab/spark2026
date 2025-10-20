#![no_main]

use core::num::NonZeroU16;

use libfuzzer_sys::fuzz_target;
use spark_core::buffer::{PoolStats, ReadableBuffer, WritableBuffer};
use spark_core::codec::{DecodeContext, EncodeContext};
use spark_core::contract::{Budget, BudgetKind};
use spark_core::error::codes;
use spark_core::{BufferPool, CoreError};

struct NoopBuffer;

impl WritableBuffer for NoopBuffer {
    fn capacity(&self) -> usize {
        0
    }

    fn remaining_mut(&self) -> usize {
        0
    }

    fn written(&self) -> usize {
        0
    }

    fn reserve(&mut self, _additional: usize) -> Result<(), CoreError> {
        Ok(())
    }

    fn put_slice(&mut self, _src: &[u8]) -> Result<(), CoreError> {
        Ok(())
    }

    fn write_from(
        &mut self,
        _src: &mut dyn ReadableBuffer,
        _len: usize,
    ) -> Result<(), CoreError> {
        Ok(())
    }

    fn clear(&mut self) {}

    fn freeze(self: Box<Self>) -> Result<Box<dyn ReadableBuffer>, CoreError> {
        Err(CoreError::new(
            codes::PROTOCOL_DECODE,
            "noop buffer used for fuzzing cannot freeze",
        ))
    }
}

struct FuzzAllocator;

impl BufferPool for FuzzAllocator {
    fn acquire(&self, _min_capacity: usize) -> Result<Box<dyn WritableBuffer>, CoreError> {
        Ok(Box::new(NoopBuffer))
    }

    fn shrink_to_fit(&self) -> Result<usize, CoreError> {
        Ok(0)
    }

    fn statistics(&self) -> Result<PoolStats, CoreError> {
        Ok(PoolStats::default())
    }
}

#[derive(Clone, Debug)]
struct Operation {
    frame_len: usize,
    flags: u8,
    depth_hint: u8,
    refund_value: u8,
}

impl Operation {
    fn wants_decode(&self) -> bool {
        self.flags & 0x01 != 0
    }

    fn wants_encode(&self) -> bool {
        self.flags & 0x02 != 0
    }

    fn refund_decode(&self) -> bool {
        self.flags & 0x04 != 0
    }

    fn refund_encode(&self) -> bool {
        self.flags & 0x08 != 0
    }

    fn refund_amount(&self) -> usize {
        usize::from(self.refund_value) + 1
    }
}

#[derive(Clone, Debug)]
struct FuzzCase {
    max_frame_size: Option<usize>,
    max_depth: Option<NonZeroU16>,
    budget: Option<u64>,
    operations: Vec<Operation>,
}

impl FuzzCase {
    fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 5 {
            return None;
        }

        let max_frame_raw = u16::from_le_bytes([data[0], data[1]]);
        let max_frame_size = if max_frame_raw == 0 {
            None
        } else {
            Some(((max_frame_raw as usize).saturating_mul(4)).min(1 << 18))
        };

        let depth_raw = data[2];
        let max_depth = if depth_raw == 0 {
            None
        } else {
            NonZeroU16::new(depth_raw as u16)
        };

        let budget_raw = u16::from_le_bytes([data[3], data[4]]);
        let budget = if budget_raw == 0 {
            None
        } else {
            Some((budget_raw as u64).saturating_mul(4).max(1))
        };

        let mut operations = Vec::new();
        let mut idx = 5;
        while idx + 3 < data.len() && operations.len() < 128 {
            let frame_len = u16::from_le_bytes([data[idx], data[idx + 1]]) as usize;
            let flags = data[idx + 2];
            let depth_refund = data[idx + 3];
            operations.push(Operation {
                frame_len,
                flags,
                depth_hint: depth_refund >> 4,
                refund_value: depth_refund & 0x0F,
            });
            idx += 4;
        }

        if operations.is_empty() {
            return None;
        }

        Some(Self {
            max_frame_size,
            max_depth,
            budget,
            operations,
        })
    }
}

fuzz_target!(|data: &[u8]| {
    if let Some(case) = parse_case_or_hex_seed(data) {
        run_case(case);
    }
});

fn parse_case_or_hex_seed(data: &[u8]) -> Option<FuzzCase> {
    if let Some(decoded) = decode_hex_seed(data) {
        FuzzCase::parse(&decoded)
    } else {
        FuzzCase::parse(data)
    }
}

/// 将 `hex: xx` 形式的 ASCII 种子转换为真实字节序列。
///
/// # 教案式注解
/// - **意图 (Why)**：仓库禁止存放二进制文件，因此以十六进制字符串保存最小触发样例，并在此处还原为模糊器需要的原始字节。
/// - **策略 (How)**：
///   1. 仅当前缀为 `hex:` 时触发解析；
///   2. 忽略空白字符，按两个字符一组读取；
///   3. 通过 [`decode_nibble`] 将 ASCII 数字/字母映射到半字节后组合；
///   4. 若遇到非法字符或奇数个半字节，返回 `None` 让上层回退到原始字节路径。
/// - **契约 (What)**：
///   - **输入**：任意字节切片；
///   - **输出**：合法十六进制字符串对应的字节数组；
///   - **前置条件**：无；
///   - **后置条件**：成功时保证返回的向量长度等于解析出的字节数。
/// - **风险提示 (Trade-offs)**：解析失败会直接返回 `None`，模糊器会继续使用原始种子，不会影响覆盖面。
fn decode_hex_seed(data: &[u8]) -> Option<Vec<u8>> {
    const PREFIX: &[u8] = b"hex:";
    if !data.starts_with(PREFIX) {
        return None;
    }

    let mut output = Vec::new();
    let mut iter = data[PREFIX.len()..]
        .iter()
        .copied()
        .filter(|byte| !byte.is_ascii_whitespace());

    loop {
        let hi = match iter.next() {
            Some(byte) => byte,
            None => break,
        };
        let lo = iter.next()?;
        let hi = decode_nibble(hi)?;
        let lo = decode_nibble(lo)?;
        output.push((hi << 4) | lo);
    }

    Some(output)
}

/// 将单个 ASCII 字符映射到对应的 4bit 数值，非法字符返回 `None`。
fn decode_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn run_case(case: FuzzCase) {
    let allocator = FuzzAllocator;
    let decode_budget = case
        .budget
        .map(|limit| Budget::new(BudgetKind::Decode, limit));
    let encode_budget = case
        .budget
        .map(|limit| Budget::new(BudgetKind::Flow, limit));

    let mut decode_ctx = DecodeContext::with_limits(
        &allocator,
        decode_budget.as_ref(),
        case.max_frame_size,
        case.max_depth,
    );
    let mut encode_ctx = EncodeContext::with_limits(
        &allocator,
        encode_budget.as_ref(),
        case.max_frame_size,
        case.max_depth,
    );

    for op in case.operations {
        let bounded_len = op.frame_len.min(1 << 17);

        if op.wants_decode() {
            let _ = decode_ctx.check_frame_constraints(bounded_len);
            if op.depth_hint > 0 {
                exercise_decode_depth(&mut decode_ctx, op.depth_hint);
            }
            if op.refund_decode() {
                decode_ctx.refund_budget(op.refund_amount());
            }
        }

        if op.wants_encode() {
            let _ = encode_ctx.check_frame_constraints(bounded_len);
            if op.depth_hint > 0 {
                exercise_encode_depth(&mut encode_ctx, op.depth_hint);
            }
            if op.refund_encode() {
                encode_ctx.refund_budget(op.refund_amount());
            }
        }
    }
}

fn exercise_decode_depth(ctx: &mut DecodeContext<'_>, depth_hint: u8) {
    if depth_hint == 0 {
        return;
    }
    if let Ok(_guard) = ctx.enter_frame() {
        exercise_decode_depth(ctx, depth_hint.saturating_sub(1));
    }
}

fn exercise_encode_depth(ctx: &mut EncodeContext<'_>, depth_hint: u8) {
    if depth_hint == 0 {
        return;
    }
    if let Ok(_guard) = ctx.enter_frame() {
        exercise_encode_depth(ctx, depth_hint.saturating_sub(1));
    }
}
