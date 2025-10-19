use criterion::{Criterion, black_box};
use spark_core::{ReadableBuffer, SparkError, WritableBuffer};
use std::{env, time::Duration};

/// 简单的基准测试：验证缓冲读写契约的往返成本。
///
/// # 设计背景（Why）
/// - 在优化 API 契约时，需要通过基准确认典型实现能够稳定支持“写 -> 冻结 -> 读”流程。
/// - 基准实现以纯 `Vec` 为后端，模拟常见的 heap 缓冲策略，便于快速检测回归。
///
/// # 逻辑解析（How）
/// - 基准循环执行：写入 1 KiB 数据、冻结为只读缓冲、读取并消费所有字节。
/// - `VecWriter`/`VecReader` 为基准内部实现，严格遵守 `ReadableBuffer`/`WritableBuffer` 的契约语义。
fn bench_buffer_roundtrip(c: &mut Criterion) {
    c.bench_function("buffer_roundtrip", |b| {
        b.iter(|| {
            let mut writer = VecWriter::default();
            writer.reserve(1024).unwrap();
            writer.put_slice(&[0u8; 512]).unwrap();
            writer.put_slice(&[1u8; 512]).unwrap();

            let mut reader = Box::new(writer).freeze().unwrap();
            let mut sink = vec![0u8; reader.remaining()];
            reader.copy_into_slice(&mut sink).unwrap();
            black_box(sink)
        });
    });
}

fn main() {
    let mut quick_mode = false;
    for arg in env::args().skip(1) {
        if arg == "--quick" {
            quick_mode = true;
        }
    }

    let mut criterion = Criterion::default();
    if quick_mode {
        criterion = criterion
            .sample_size(10)
            .warm_up_time(Duration::from_millis(100))
            .measurement_time(Duration::from_millis(250));
    }

    bench_buffer_roundtrip(&mut criterion);
    criterion.final_summary();
}

#[derive(Default)]
struct VecWriter {
    data: Vec<u8>,
}

#[derive(Clone)]
struct VecReader {
    data: Vec<u8>,
    read: usize,
}

impl ReadableBuffer for VecReader {
    fn remaining(&self) -> usize {
        self.data.len() - self.read
    }

    fn chunk(&self) -> &[u8] {
        &self.data[self.read..]
    }

    fn split_to(&mut self, len: usize) -> Result<Box<dyn ReadableBuffer>, SparkError> {
        if len > self.remaining() {
            return Err(SparkError::new(
                "buffer.out_of_range",
                "split_to 超出剩余长度",
            ));
        }
        let segment_data = self.data[self.read..self.read + len].to_vec();
        let segment = VecReader {
            data: segment_data,
            read: 0,
        };
        self.read += len;
        Ok(Box::new(segment))
    }

    fn advance(&mut self, len: usize) -> Result<(), SparkError> {
        if len > self.remaining() {
            return Err(SparkError::new(
                "buffer.out_of_range",
                "advance 超出剩余长度",
            ));
        }
        self.read += len;
        Ok(())
    }

    fn copy_into_slice(&mut self, dst: &mut [u8]) -> Result<(), SparkError> {
        if dst.len() > self.remaining() {
            return Err(SparkError::new(
                "buffer.out_of_range",
                "copy_into_slice 目标长度超出",
            ));
        }
        let end = self.read + dst.len();
        dst.copy_from_slice(&self.data[self.read..end]);
        self.read = end;
        Ok(())
    }

    fn try_into_vec(self: Box<Self>) -> Result<Vec<u8>, SparkError> {
        let VecReader { data, read } = *self;
        Ok(data[read..].to_vec())
    }
}

impl WritableBuffer for VecWriter {
    fn capacity(&self) -> usize {
        self.data.capacity()
    }

    fn remaining_mut(&self) -> usize {
        self.capacity() - self.data.len()
    }

    fn written(&self) -> usize {
        self.data.len()
    }

    fn reserve(&mut self, additional: usize) -> Result<(), SparkError> {
        self.data
            .try_reserve(additional)
            .map_err(|_| SparkError::new("buffer.reserve_failed", "Vec reserve 失败"))
    }

    fn put_slice(&mut self, src: &[u8]) -> Result<(), SparkError> {
        self.data.extend_from_slice(src);
        Ok(())
    }

    fn write_from(&mut self, src: &mut dyn ReadableBuffer, len: usize) -> Result<(), SparkError> {
        let segment = src.split_to(len)?;
        let chunk = segment.try_into_vec()?;
        self.data.extend_from_slice(&chunk);
        Ok(())
    }

    fn clear(&mut self) {
        self.data.clear();
    }

    fn freeze(self: Box<Self>) -> Result<Box<dyn ReadableBuffer>, SparkError> {
        let VecWriter { data } = *self;
        Ok(Box::new(VecReader { data, read: 0 }))
    }
}
