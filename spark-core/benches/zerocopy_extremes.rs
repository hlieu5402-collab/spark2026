use serde::{Deserialize, Serialize};
use spark_core::buffer::BufView;
use std::env;
use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// `zerocopy_extremes` 基准：在分片数量与单片尺寸的极端组合下，测量 `BufView::as_chunks`
/// 暴露零拷贝窗口的稳定性，并对 P99 延迟实施阈值守门。
///
/// # 教案式摘要
/// - **意图 (Why)**：BufView 契约承诺“任意分片布局下均不额外拷贝”，本基准专门聚焦
///   “单片超大”与“超多微片”两种极端，锁定 P99 延迟，避免未来重构意外引入线性额外
///   开销。
/// - **定位 (Where)**：作为 `cargo bench -- --quick` 的组成部分，运行在 `spark-core`
///   crate 内的独立二进制，生成 `docs/reports/benchmarks/zerocopy_extremes.{quick,full}.json`。
/// - **执行逻辑 (How)**：针对每个场景多次调用 `BufView::as_chunks` 并遍历所有分片，采集
///   “每 KB 消耗的纳秒”样本，计算 P50/P95/P99 等统计量。
/// - **契约 (What)**：
///   - **输入参数**：支持 `--quick`、`--output <path>`、`--threshold <path>` 命令行参数；
///     其余参数视为非法。
///   - **输出**：标准输出打印关键指标，同时将统计结果写入 JSON 文件；若 P99 超过阈值，
///     立即返回错误码。
///   - **前置条件**：运行环境需提供 `std`，并允许在 `docs/reports/benchmarks` 下创建文件。
///   - **后置条件**：若阈值校验通过，则保证两种极端场景的 `p99_ns_per_kb` 不超过配置。
/// - **设计权衡 (Trade-offs)**：为了让数据具有实际意义，基准会在内部重新分配 `Vec<&[u8]>`
///   保存分片指针；虽然这会带来额外指针复制，但它与真实业务实现使用
///   `Chunks::from_vec` 时的代价一致。
fn main() {
    if let Err(error) = run() {
        eprintln!("zerocopy_extremes_error={error}");
        std::process::exit(1);
    }
}

/// 统一的错误类型，覆盖参数解析、IO 与阈值比较等失败场景。
///
/// # Why
/// - 避免在主流程中频繁使用 `expect`/`unwrap`，让错误信息对 CI 更友好。
///
/// # How
/// - 枚举化不同错误来源，`Display` 实现提供人类可读描述。
///
/// # What
/// - `Cli(String)`：命令行格式错误或缺少参数。
/// - `Io(std::io::Error)`：读写文件失败。
/// - `Serde(serde_json::Error)`：阈值 JSON 解析失败。
/// - `Threshold(String)`：P99 校验失败时的诊断信息。
#[derive(Debug)]
enum BenchError {
    Cli(String),
    Io(std::io::Error),
    Serde(serde_json::Error),
    Threshold(String),
}

impl std::fmt::Display for BenchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BenchError::Cli(msg) => write!(f, "CLI 参数错误: {msg}"),
            BenchError::Io(err) => write!(f, "IO 失败: {err}"),
            BenchError::Serde(err) => write!(f, "JSON 解析失败: {err}"),
            BenchError::Threshold(msg) => write!(f, "阈值校验失败: {msg}"),
        }
    }
}

impl std::error::Error for BenchError {}

impl From<std::io::Error> for BenchError {
    fn from(value: std::io::Error) -> Self {
        BenchError::Io(value)
    }
}

impl From<serde_json::Error> for BenchError {
    fn from(value: serde_json::Error) -> Self {
        BenchError::Serde(value)
    }
}

/// 解析命令行参数，生成基准运行配置。
///
/// # Why
/// - 允许开发者通过 `--quick` 控制迭代次数，通过 `--output`/`--threshold` 指定自定义路径。
///
/// # How
/// - 逐个遍历 `env::args`，根据关键字匹配并写入配置结构体；遇到未知参数立即报错。
///
/// # What
/// - 返回 `CliOptions`，包含是否快速模式以及可选的输出/阈值覆盖路径。
/// - 前置条件：参数成对出现（例如 `--output` 后必须跟路径）。
/// - 后置条件：若返回成功，`args` 中的所有标志均被识别并消费。
fn parse_cli() -> Result<CliOptions, BenchError> {
    let mut quick_mode = false;
    let mut output = None;
    let mut threshold = None;

    let mut iter = env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--quick" => {
                quick_mode = true;
            }
            "--bench" => {
                // Cargo 在执行 `cargo bench -p crate --bench name` 时会追加 `--bench` 参数，
                // 不携带任何值。基准无需处理该标志，直接忽略即可。
            }
            "--output" => {
                let value = iter
                    .next()
                    .ok_or_else(|| BenchError::Cli("--output 之后缺少路径".into()))?;
                output = Some(PathBuf::from(value));
            }
            "--threshold" => {
                let value = iter
                    .next()
                    .ok_or_else(|| BenchError::Cli("--threshold 之后缺少路径".into()))?;
                threshold = Some(PathBuf::from(value));
            }
            other => {
                return Err(BenchError::Cli(format!("不支持的参数: {other}")));
            }
        }
    }

    Ok(CliOptions {
        quick_mode,
        output,
        threshold,
    })
}

/// CLI 解析结果。
#[derive(Default)]
struct CliOptions {
    quick_mode: bool,
    output: Option<PathBuf>,
    threshold: Option<PathBuf>,
}

/// 基准配置，描述迭代次数与批处理大小。
///
/// # Why
/// - 为 `quick/full` 模式分别提供合适的采样密度，兼顾执行时间与统计稳定性。
///
/// # How
/// - `iterations`：总调用次数；
/// - `batch_size`：将多次调用聚合为一个计时窗口，降低计时抖动。
///
/// # What
/// - `quick_mode`：标识是否为快速模式。
/// - 前置条件：`iterations` 必须大于 0；`batch_size` 至少为 1。
/// - 后置条件：调用方需保证 `iterations` 能被批量循环消费（余数由内部处理）。
#[derive(Clone, Copy)]
struct BenchConfig {
    iterations: u64,
    batch_size: u64,
    quick_mode: bool,
}

/// 单个样本的观测值。
#[derive(Clone, Copy)]
struct SamplePoint {
    ns_per_iter: u64,
    ns_per_kb: f64,
}

/// 基准中使用的场景定义，包含名称与底层数据形态。
///
/// # Why
/// - 将“单片超大”“超多微片”抽象为统一结构，便于后续遍历与报告生成。
///
/// # How
/// - `fixture` 保存底层缓冲，`total_bytes`/`expected_chunks` 提供契约校验所需的元信息。
///
/// # What
/// - `name`：场景名称，用于日志与阈值匹配。
/// - `total_bytes`：每次遍历应观察到的总字节数。
/// - `expected_chunks`：理论分片数量，保证实现未擅自折叠。
struct ScenarioSpec {
    name: &'static str,
    fixture: ViewFixture,
    total_bytes: usize,
    expected_chunks: usize,
}

impl ScenarioSpec {
    fn new(name: &'static str, fixture: ViewFixture) -> Self {
        let total_bytes = fixture.total_len();
        let expected_chunks = fixture.expected_chunks();
        Self {
            name,
            fixture,
            total_bytes,
            expected_chunks,
        }
    }
}

/// 场景中用到的缓冲类型枚举。
///
/// # Why
/// - 需要同时覆盖 `Vec<u8>`（单片）与自定义 `ScatterView`（多片）两种布局。
///
/// # How
/// - 通过 `as_view` 返回统一的 `&dyn BufView` 接口。
///
/// # What
/// - `Linear(Vec<u8>)`：单片连续缓冲。
/// - `Scatter(ScatterView)`：按配置拆分的多片缓冲。
enum ViewFixture {
    Linear(Vec<u8>),
    Scatter(ScatterView),
}

impl ViewFixture {
    fn total_len(&self) -> usize {
        match self {
            ViewFixture::Linear(data) => data.len(),
            ViewFixture::Scatter(view) => view.total_len,
        }
    }

    fn expected_chunks(&self) -> usize {
        match self {
            ViewFixture::Linear(data) => {
                if data.is_empty() {
                    0
                } else {
                    1
                }
            }
            ViewFixture::Scatter(view) => view.chunk_count,
        }
    }
}

impl BufView for ViewFixture {
    fn as_chunks(&self) -> spark_core::buffer::Chunks<'_> {
        match self {
            ViewFixture::Linear(data) => spark_core::buffer::Chunks::from_single(data.as_slice()),
            ViewFixture::Scatter(view) => view.as_chunks(),
        }
    }

    fn len(&self) -> usize {
        self.total_len()
    }
}

/// 多分片缓冲视图实现，模拟极端 scatter/gather 场景。
///
/// # Why
/// - 真实业务中会出现“碎片化极多但总量有限”的情况，例如基于 `VecDeque` 或 ring buffer
///   的收发队列；该结构用于复现这类行为。
///
/// # How
/// - 构造时按给定每片大小生成 `Box<[u8]>`，`as_chunks` 通过指针向量零拷贝暴露。
///
/// # What
/// - `total_len`：缓存的总字节数，用于契约校验。
/// - `chunk_count`：分片数量，帮助阈值报告。
/// - 前置条件：构造参数必须保证 `chunk_len * chunk_count == total_len`。
/// - 后置条件：`as_chunks` 返回的切片顺序与构造时一致。
struct ScatterView {
    shards: Vec<Box<[u8]>>,
    total_len: usize,
    chunk_count: usize,
}

impl ScatterView {
    /// 构造多分片缓冲。
    ///
    /// # Why
    /// - 通过统一的工厂方法封装初始化细节，使得基准在不同分片规模下复用同一实现。
    ///
    /// # How
    /// - 为每个分片分配一段 `Box<[u8]>`，首字节写入索引用于调试；整体容量与输入参数成正比。
    ///
    /// # What
    /// - **输入**：`chunk_len` 为单片长度，`chunk_count` 为分片数量。
    /// - **前置条件**：二者均需大于 0；否则直接 panic，避免产生空视图干扰基准。
    /// - **后置条件**：`total_len` 与 `chunk_count` 与输入一致，且所有分片独立存储。
    ///
    /// # Trade-offs
    /// - 为了确保零拷贝语义，每次构造都会真实分配 `chunk_count` 个盒装切片，会占用额外内存；
    ///   基准默认使用 4K 级别的分片数量以避免 OOM。
    fn new(chunk_len: usize, chunk_count: usize) -> Self {
        assert!(chunk_len > 0, "chunk_len 必须大于 0");
        assert!(chunk_count > 0, "chunk_count 必须大于 0");
        let mut shards = Vec::with_capacity(chunk_count);
        for idx in 0..chunk_count {
            let mut data = vec![0u8; chunk_len];
            data[0] = (idx & 0xFF) as u8;
            shards.push(data.into_boxed_slice());
        }
        Self {
            shards,
            total_len: chunk_len * chunk_count,
            chunk_count,
        }
    }
}

impl BufView for ScatterView {
    fn as_chunks(&self) -> spark_core::buffer::Chunks<'_> {
        let mut slices = Vec::with_capacity(self.shards.len());
        for shard in &self.shards {
            slices.push(shard.as_ref());
        }
        spark_core::buffer::Chunks::from_vec(slices)
    }

    fn len(&self) -> usize {
        self.total_len
    }
}

/// 延迟统计摘要，用于 JSON 序列化。
#[derive(Clone, Copy, Debug, Serialize)]
struct LatencySummaryF64 {
    p50: f64,
    p95: f64,
    p99: f64,
    mean: f64,
    min: f64,
    max: f64,
}

#[derive(Clone, Copy, Debug, Serialize)]
struct LatencySummaryU64 {
    p50: u64,
    p95: u64,
    p99: u64,
    mean: f64,
    min: u64,
    max: u64,
}

/// 单场景基准结果。
#[derive(Debug, Serialize)]
struct ScenarioReport {
    name: &'static str,
    total_bytes: usize,
    expected_chunks: usize,
    samples: u64,
    ns_per_iter: LatencySummaryU64,
    ns_per_kb: LatencySummaryF64,
}

/// 总体基准报告。
#[derive(Debug, Serialize)]
struct BenchmarkReport {
    benchmark: &'static str,
    quick_mode: bool,
    iterations: u64,
    batch_size: u64,
    scenarios: Vec<ScenarioReport>,
}

/// 阈值文件结构。
#[derive(Debug, Deserialize)]
struct ThresholdFile {
    benchmark: String,
    modes: Vec<ModeThreshold>,
}

#[derive(Debug, Deserialize)]
struct ModeThreshold {
    quick_mode: bool,
    scenarios: Vec<ScenarioThreshold>,
}

#[derive(Debug, Deserialize)]
struct ScenarioThreshold {
    name: String,
    max_p99_ns_per_kb: f64,
    max_mean_ns_per_kb: Option<f64>,
}

/// 基准主流程：解析 CLI、运行场景、生成报告并执行阈值校验。
///
/// # Why
/// - 集中处理生命周期，确保执行顺序固定：先采样、再校验、最后持久化。
///
/// # How
/// 1. 调用 [`parse_cli`] 获取运行模式与路径覆盖；
/// 2. 根据模式构造 [`BenchConfig`]；
/// 3. 遍历 [`build_scenarios`] 返回的场景并采样；
/// 4. 读取阈值文件、执行 [`enforce_thresholds`]；
/// 5. 将结果写入 JSON。
///
/// # What
/// - 返回 `Result<(), BenchError>`：成功时说明阈值满足，失败时携带诊断信息。
/// - 前置条件：工作目录位于仓库根目录附近，使默认路径可解析。
/// - 后置条件：若成功，`docs/reports/benchmarks` 中会出现最新报告。
fn run() -> Result<(), BenchError> {
    let cli = parse_cli()?;
    let config = if cli.quick_mode {
        BenchConfig {
            iterations: 6_400,
            batch_size: 32,
            quick_mode: true,
        }
    } else {
        BenchConfig {
            iterations: 64_000,
            batch_size: 128,
            quick_mode: false,
        }
    };

    let scenarios = build_scenarios();
    let mut reports = Vec::with_capacity(scenarios.len());

    for scenario in &scenarios {
        let report = measure_scenario(scenario, config);
        println!(
            "scenario={} p50_ns_per_kb={:.3} p99_ns_per_kb={:.3}",
            scenario.name, report.ns_per_kb.p50, report.ns_per_kb.p99
        );
        reports.push(report);
    }

    let report = BenchmarkReport {
        benchmark: "zerocopy_extremes",
        quick_mode: config.quick_mode,
        iterations: config.iterations,
        batch_size: config.batch_size,
        scenarios: reports,
    };

    let thresholds = load_thresholds(cli.threshold.as_deref(), config.quick_mode)?;
    enforce_thresholds(&report, &thresholds)?;
    persist_report(&report, cli.output.as_deref())?;

    Ok(())
}

/// 构造基准场景列表。
///
/// # Why
/// - 将极端分片模式固定在代码中，避免不同开发者在本地选择不同的组合导致结果不可对比。
///
/// # How
/// - 选择 512 KiB 单片作为“最大连续块”场景；
/// - 选择 128 B × 4096 片作为“海量微片”场景，确保总字节数一致。
///
/// # What
/// - 返回按顺序排列的 [`ScenarioSpec`] 向量。
/// - 前置条件：内存足够容纳 512 KiB + 512 KiB 数据。
/// - 后置条件：两个场景总字节数相同，可直接比较 `ns/KB` 指标。
fn build_scenarios() -> Vec<ScenarioSpec> {
    let single_large = ScenarioSpec::new(
        "single_large_chunk",
        ViewFixture::Linear(vec![0u8; 512 * 1024]),
    );

    let scatter = ScenarioSpec::new(
        "many_tiny_chunks",
        ViewFixture::Scatter(ScatterView::new(128, 4096)),
    );

    vec![single_large, scatter]
}

/// 针对单个场景执行测量。
///
/// # Why
/// - 获取 `BufView::as_chunks` 在固定数据布局下的延迟分布，支持后续阈值比较。
///
/// # How
/// - 按批次循环调用 [`consume_view`]，对批次耗时做整数除法得到每次调用耗时；
/// - 将耗时换算为 `ns/KB` 存入样本；
/// - 调用统计函数生成分位数。
///
/// # What
/// - 输入：`scenario` 提供数据与期望，`config` 控制迭代总量。
/// - 输出：`ScenarioReport`，包含样本数量与两套延迟摘要。
/// - 前置条件：`scenario.total_bytes` 与 `expected_chunks` 真实反映底层数据；
/// - 后置条件：样本长度等于批次数，每个样本至少包含一次迭代。
fn measure_scenario(scenario: &ScenarioSpec, config: BenchConfig) -> ScenarioReport {
    let mut samples = Vec::with_capacity((config.iterations / config.batch_size) as usize + 1);

    let mut remaining = config.iterations;
    while remaining > 0 {
        let chunk = remaining.min(config.batch_size);
        let started = Instant::now();
        for _ in 0..chunk {
            consume_view(scenario);
        }
        let elapsed = started.elapsed().as_nanos();
        let per_iter = ((elapsed + (chunk as u128 / 2)) / chunk as u128) as u64;
        let bytes = scenario.total_bytes as f64;
        let ns_per_kb = if bytes == 0.0 {
            0.0
        } else {
            (per_iter as f64) / (bytes / 1024.0)
        };
        samples.push(SamplePoint {
            ns_per_iter: per_iter,
            ns_per_kb,
        });
        remaining -= chunk;
    }

    let ns_per_iter = analyze_ns(&samples);
    let ns_per_kb = analyze_ns_per_kb(&samples);

    ScenarioReport {
        name: scenario.name,
        total_bytes: scenario.total_bytes,
        expected_chunks: scenario.expected_chunks,
        samples: samples.len() as u64,
        ns_per_iter,
        ns_per_kb,
    }
}

/// 消费 BufView，统计分片数量与字节数。
///
/// # Why
/// - 在基准过程中验证 `BufView` 的契约是否成立，防止实现悄然折叠分片或丢字节。
///
/// # How
/// - 通过 `as_chunks` 迭代全部分片，累计字节数与分片数，使用 `black_box` 阻止优化。
///
/// # What
/// - 输入：[`ScenarioSpec`]，提供期望的总字节与分片数。
/// - 前置条件：`fixture` 必须实现 `BufView`，且在迭代期间保持只读。
/// - 后置条件：函数返回前若契约不满足将 panic，从而终止基准并提示实现缺陷。
fn consume_view(scenario: &ScenarioSpec) {
    let view = &scenario.fixture;
    let mut observed_bytes = 0usize;
    let mut observed_chunks = 0usize;
    for chunk in view.as_chunks() {
        observed_bytes += chunk.len();
        observed_chunks += 1;
        black_box(chunk);
    }
    assert_eq!(observed_bytes, scenario.total_bytes, "分片长度总和不匹配");
    assert_eq!(observed_chunks, scenario.expected_chunks, "分片数量不匹配");
    black_box(observed_chunks);
}

/// 计算“每次调用耗时 (ns)”的统计量。
///
/// # Why
/// - 原始纳秒数据可帮助确认 Batch 均摊是否合理，同时为分析器提供绝对耗时基线。
///
/// # How
/// - 将样本排序后计算 P50/P95/P99，`mean` 使用 `u128` 累加避免溢出。
///
/// # What
/// - 输入：按批次收集的样本数组。
/// - 输出：[`LatencySummaryU64`]，记录分位数与均值。
/// - 前置条件：样本可为空（空时返回全 0），调用方需自行确保批次>0。
/// - 后置条件：排序在本地 Vec 上进行，不会修改原输入。
fn analyze_ns(samples: &[SamplePoint]) -> LatencySummaryU64 {
    let mut values: Vec<u64> = samples.iter().map(|s| s.ns_per_iter).collect();
    values.sort_unstable();
    let min = *values.first().unwrap_or(&0);
    let max = *values.last().unwrap_or(&0);
    let mean = if values.is_empty() {
        0.0
    } else {
        let sum: u128 = values.iter().map(|&v| v as u128).sum();
        (sum as f64) / (values.len() as f64)
    };
    LatencySummaryU64 {
        p50: percentile_u64(&values, 0.50),
        p95: percentile_u64(&values, 0.95),
        p99: percentile_u64(&values, 0.99),
        mean,
        min,
        max,
    }
}

/// 计算“每 KB 耗时 (ns/KB)”的统计量。
///
/// # Why
/// - 阈值以 `ns/KB` 定义，需要单独的统计过程来规避舍入误差。
///
/// # How
/// - 复制样本后按浮点排序，使用线性插值计算分位数。
///
/// # What
/// - 输入：与 [`analyze_ns`] 相同的样本数组。
/// - 输出：[`LatencySummaryF64`]。
/// - 前置条件：样本可能包含 `0.0`，排序需处理浮点比较。
/// - 后置条件：统计过程中不修改原样本。
fn analyze_ns_per_kb(samples: &[SamplePoint]) -> LatencySummaryF64 {
    let mut values: Vec<f64> = samples.iter().map(|s| s.ns_per_kb).collect();
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let min = *values.first().unwrap_or(&0.0);
    let max = *values.last().unwrap_or(&0.0);
    let mean = if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / (values.len() as f64)
    };
    LatencySummaryF64 {
        p50: percentile_f64(&values, 0.50),
        p95: percentile_f64(&values, 0.95),
        p99: percentile_f64(&values, 0.99),
        mean,
        min,
        max,
    }
}

/// 计算整型样本的分位数，使用线性插值避免阶梯效应。
///
/// # Why
/// - Benchmark 需要 P99 等非整数分位数，直接取下界会低估真实延迟。
///
/// # How
/// - 根据 `quantile` 计算浮点 rank，向下/向上取整后线性插值，最终四舍五入回 `u64`。
///
/// # What
/// - 输入：已排序的样本数组与目标分位数（0..=1）。
/// - 输出：估计值；当样本为空时返回 0。
/// - 风险：若数组未排序，将产生错误结果；调用方需事先保证排序。
fn percentile_u64(sorted: &[u64], quantile: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (quantile.clamp(0.0, 1.0)) * ((sorted.len() - 1) as f64);
    let lower = rank.floor() as usize;
    let upper = rank.ceil() as usize;
    if lower == upper {
        sorted[lower]
    } else {
        let weight = rank - lower as f64;
        let low = sorted[lower] as f64;
        let high = sorted[upper] as f64;
        (low + (high - low) * weight).round() as u64
    }
}

/// 计算浮点样本的分位数。
///
/// # Why
/// - `ns/KB` 指标为浮点，需要保持高精度插值，避免 P99 判定时出现过度保守的误差。
///
/// # How
/// - 与 [`percentile_u64`] 相同，采用 rank 插值。
///
/// # What
/// - 输入：已排序的 `f64` 数组与分位数。
/// - 输出：对应的估计值，空数组返回 0.0。
/// - 注意：浮点排序依赖 `partial_cmp`，若出现 NaN 将 panic；基准内部不生成 NaN。
fn percentile_f64(sorted: &[f64], quantile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (quantile.clamp(0.0, 1.0)) * ((sorted.len() - 1) as f64);
    let lower = rank.floor() as usize;
    let upper = rank.ceil() as usize;
    if lower == upper {
        sorted[lower]
    } else {
        let weight = rank - lower as f64;
        sorted[lower] + (sorted[upper] - sorted[lower]) * weight
    }
}

/// 读取阈值配置，返回与当前模式匹配的规则。
///
/// # Why
/// - 让 CI 可以通过更新 JSON 文件调整 SLO，而无需改动 Rust 源码。
///
/// # How
/// - 确定阈值文件路径（命令行覆盖 > 默认）；
/// - 使用 `serde_json` 解析为 [`ThresholdFile`]；
/// - 按 `quick_mode` 选择对应配置。
///
/// # What
/// - 输入：可选路径与模式标识。
/// - 输出：[`ModeThreshold`]，供 [`enforce_thresholds`] 使用。
/// - 前置条件：JSON 顶层 `benchmark` 必须等于 `zerocopy_extremes`。
/// - 后置条件：若模式缺失或名称不匹配，返回 `BenchError::Threshold`。
fn load_thresholds(path: Option<&Path>, quick_mode: bool) -> Result<ModeThreshold, BenchError> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir.parent().expect("crate placed under repo root");
    let default_path = repo_root
        .join("docs")
        .join("reports")
        .join("benchmarks")
        .join("zerocopy_extremes.thresholds.json");
    let target_path = path.unwrap_or(default_path.as_path());
    let content = fs::read_to_string(target_path)?;
    let thresholds: ThresholdFile = serde_json::from_str(&content)?;
    if thresholds.benchmark != "zerocopy_extremes" {
        return Err(BenchError::Threshold(format!(
            "阈值文件基准名称不匹配: {}",
            thresholds.benchmark
        )));
    }
    thresholds
        .modes
        .into_iter()
        .find(|mode| mode.quick_mode == quick_mode)
        .ok_or_else(|| {
            BenchError::Threshold(format!(
                "阈值文件缺少 {} 模式配置",
                if quick_mode { "quick" } else { "full" }
            ))
        })
}

/// 将测量结果与阈值进行对比，超出则报错。
///
/// # Why
/// - 在 CI 中自动守护“P99 ≤ 既定目标”的承诺，避免性能回退默默溜走。
///
/// # How
/// - 对每个场景查找阈值项，分别比较 `p99` 与可选的 `mean`；
/// - 收集所有违规项，统一输出，便于一次性修复多个问题。
///
/// # What
/// - 输入：测量报告与对应阈值。
/// - 输出：成功或 `BenchError::Threshold`，其中包含违规详情。
/// - 前置条件：阈值文件需覆盖所有场景。
/// - 后置条件：若函数返回 `Ok(())`，说明所有场景均满足 SLO。
fn enforce_thresholds(
    report: &BenchmarkReport,
    thresholds: &ModeThreshold,
) -> Result<(), BenchError> {
    let mut violations = Vec::new();
    for scenario in &report.scenarios {
        let Some(limit) = thresholds
            .scenarios
            .iter()
            .find(|entry| entry.name == scenario.name)
        else {
            return Err(BenchError::Threshold(format!(
                "阈值文件未定义场景 {}",
                scenario.name
            )));
        };

        if scenario.ns_per_kb.p99 > limit.max_p99_ns_per_kb {
            violations.push(format!(
                "场景 {} 的 P99 {:.3}ns/KB 超过阈值 {:.3}ns/KB",
                scenario.name, scenario.ns_per_kb.p99, limit.max_p99_ns_per_kb
            ));
        }
        if let Some(max_mean) = limit.max_mean_ns_per_kb
            && scenario.ns_per_kb.mean > max_mean
        {
            violations.push(format!(
                "场景 {} 的均值 {:.3}ns/KB 超过阈值 {:.3}ns/KB",
                scenario.name, scenario.ns_per_kb.mean, max_mean
            ));
        }
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(BenchError::Threshold(violations.join("; ")))
    }
}

/// 按约定路径输出基准结果 JSON。
///
/// # Why
/// - 固化最新数据，便于文档引用与历史对比，同时供外部可视化工具消费。
///
/// # How
/// - 根据模式选择默认文件名，可被 `--output` 覆盖；
/// - 确保目录存在后写入 `serde_json` 序列化结果。
///
/// # What
/// - 输入：基准报告与可选覆盖路径。
/// - 输出：成功或 IO/序列化错误。
/// - 前置条件：文件系统允许创建/覆盖目标路径。
/// - 后置条件：若成功，会在标准输出打印 `report_path=` 便于 CI 抓取。
fn persist_report(
    report: &BenchmarkReport,
    override_path: Option<&Path>,
) -> Result<(), BenchError> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir.parent().expect("crate placed under repo root");
    let default_file = if report.quick_mode {
        "zerocopy_extremes.quick.json"
    } else {
        "zerocopy_extremes.full.json"
    };
    let default_path = repo_root
        .join("docs")
        .join("reports")
        .join("benchmarks")
        .join(default_file);
    let output_path = override_path.map(PathBuf::from).unwrap_or(default_path);
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = fs::File::create(&output_path)?;
    serde_json::to_writer_pretty(file, report)?;
    println!("report_path={}", output_path.display());
    Ok(())
}
