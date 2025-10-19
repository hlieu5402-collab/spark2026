use alloc::{string::String, vec::Vec};
use core::fmt;

/// 链路追踪上下文，遵循 W3C Trace Context 规范。
///
/// # 设计背景（Why）
/// - 汇聚 OpenTelemetry、AWS X-Ray、Jaeger 等系统的核心语义，提供跨平台可互操作的追踪标识。
/// - 结合学术界关于分布式追踪一致性的研究（如 *Always-on Distributed Tracing*），确保在高并发场景下仍具可推理性。
///
/// # 逻辑解析（How）
/// - `trace_id` 与 `span_id` 均为定长字节数组，分别对应 128/64 bit 标识。
/// - `trace_flags` 使用 [`TraceFlags`] 管理采样等控制位。
/// - `trace_state` 使用 [`TraceState`] 携带供应商扩展信息，符合 W3C `tracestate` 语法。
///
/// # 契约说明（What）
/// - **前置条件**：`trace_id` 与 `span_id` 均不得为全零；调用方应使用加密安全或全局唯一的 ID 生成策略。
/// - **后置条件**：实例可安全克隆；`trace_state` 在派生子 Span 时会被复制，以满足上下游一致性。
///
/// # 风险提示（Trade-offs）
/// - 未内置 ID 生成逻辑，避免对特定随机源做出假设；如需默认实现可在上层注入。
/// - `trace_state` 拥有堆分配开销，建议在热路径复用缓冲或限制条目数量（W3C 推荐 <= 32）。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TraceContext {
    pub trace_id: [u8; Self::TRACE_ID_LENGTH],
    pub span_id: [u8; Self::SPAN_ID_LENGTH],
    pub trace_flags: TraceFlags,
    pub trace_state: TraceState,
}

impl TraceContext {
    /// Trace ID 的长度（字节）。
    pub const TRACE_ID_LENGTH: usize = 16;
    /// Span ID 的长度（字节）。
    pub const SPAN_ID_LENGTH: usize = 8;

    /// 创建新的追踪上下文。
    ///
    /// # 契约说明
    /// - **输入参数**：
    ///   - `trace_id`：128 bit 链路标识。
    ///   - `span_id`：64 bit Span 标识。
    ///   - `trace_flags`：采样标志集合。
    /// - **前置条件**：调用方需确保 `trace_id`、`span_id` 均符合随机性要求；必要时在调试环境放宽限制。
    /// - **后置条件**：返回值携带空的 `trace_state`，可通过 [`TraceContext::with_state`] 追加供应商扩展信息。
    ///
    /// # 风险提示
    /// - 若未遵守唯一性，将导致跨服务 Trace 聚合错误；建议在持续集成中加入契约测试确保 ID 生成逻辑正确。
    pub fn new(
        trace_id: [u8; Self::TRACE_ID_LENGTH],
        span_id: [u8; Self::SPAN_ID_LENGTH],
        trace_flags: TraceFlags,
    ) -> Self {
        Self {
            trace_id,
            span_id,
            trace_flags,
            trace_state: TraceState::default(),
        }
    }

    /// 追加 `tracestate` 信息。
    ///
    /// # 契约说明
    /// - **输入参数**：`state` 必须满足 W3C `key=value` 编码约束，详见 [`TraceState`] 文档。
    /// - **前置条件**：调用方保证 `state` 条目已按照供应商优先级排序。
    /// - **后置条件**：返回新上下文，保留原始 `trace_id`、`span_id` 与 `trace_flags`。
    pub fn with_state(mut self, state: TraceState) -> Self {
        self.trace_state = state;
        self
    }

    /// 根据当前上下文派生子 Span。
    ///
    /// # 契约说明
    /// - **输入参数**：`child_span_id` 必须为新生成的 64 bit ID。
    /// - **前置条件**：`child_span_id` 不得与当前 `span_id` 相同，避免产生重复节点。
    /// - **后置条件**：新上下文继承 `trace_id`、`trace_flags`、`trace_state`，但 `span_id` 替换为给定值。
    pub fn child_context(&self, child_span_id: [u8; Self::SPAN_ID_LENGTH]) -> Self {
        Self {
            trace_id: self.trace_id,
            span_id: child_span_id,
            trace_flags: self.trace_flags,
            trace_state: self.trace_state.clone(),
        }
    }

    /// 判断当前 Span 是否被采样。
    pub fn is_sampled(&self) -> bool {
        self.trace_flags.is_sampled()
    }

    /// 返回一个标记为采样的上下文副本。
    pub fn mark_sampled(mut self) -> Self {
        self.trace_flags.set_sampled(true);
        self
    }

    /// 校验上下文是否满足基础合法性（ID 非全零，`TraceState` 不包含重复键等）。
    pub fn validate(&self) -> Result<(), TraceContextError> {
        if self.trace_id.iter().all(|byte| *byte == 0) {
            return Err(TraceContextError::InvalidTraceId);
        }
        if self.span_id.iter().all(|byte| *byte == 0) {
            return Err(TraceContextError::InvalidSpanId);
        }
        self.trace_state.validate()?;
        Ok(())
    }
}

/// 链路追踪上下文校验失败时的错误类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceContextError {
    InvalidTraceId,
    InvalidSpanId,
    InvalidTraceState(TraceStateError),
}

impl fmt::Display for TraceContextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TraceContextError::InvalidTraceId => f.write_str("trace_id 全零或不合法"),
            TraceContextError::InvalidSpanId => f.write_str("span_id 全零或不合法"),
            TraceContextError::InvalidTraceState(err) => write!(f, "trace_state 不合法: {}", err),
        }
    }
}

/// 追踪标志位集合。
///
/// # 设计背景（Why）
/// - 对齐 W3C Trace Context 中的 `trace-flags` 定义，确保与主流采样器兼容。
///
/// # 契约说明（What）
/// - Bit0 (`0x01`) 表示 `sampled`。
/// - 其他位保留给未来扩展，应保持为 0，除非双方约定额外语义。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TraceFlags {
    bits: u8,
}

impl TraceFlags {
    /// `sampled` 位的掩码。
    pub const SAMPLED: u8 = 0x01;

    /// 创建新的标志集合。
    pub const fn new(bits: u8) -> Self {
        Self { bits }
    }

    /// 判断是否采样。
    pub fn is_sampled(&self) -> bool {
        (self.bits & Self::SAMPLED) != 0
    }

    /// 设置采样位。
    pub fn set_sampled(&mut self, sampled: bool) {
        if sampled {
            self.bits |= Self::SAMPLED;
        } else {
            self.bits &= !Self::SAMPLED;
        }
    }

    /// 获取底层比特值。
    pub const fn bits(&self) -> u8 {
        self.bits
    }
}

/// `tracestate` 条目集合。
///
/// # 设计背景（Why）
/// - 吸收 Google Cloud、Azure Monitor 等平台对于供应商优先级的经验，保留原始顺序以确保下游解析一致。
/// - 支持空集合，用于学术实验中的最小化追踪上下文。
///
/// # 逻辑解析（How）
/// - 内部使用 `Vec<TraceStateEntry>` 存储，维护插入顺序。
/// - `insert` 会根据键名去重，符合 W3C “最新值优先” 的定义。
///
/// # 契约说明（What）
/// - **前置条件**：键名必须匹配正则 `^[a-z0-9][-a-z0-9\*_\/]\w{0,255}$`，值长度不超过 256 字节。
/// - **后置条件**：`TraceState` 保证不出现重复键。
///
/// # 风险提示（Trade-offs）
/// - 未强制排序；如需特定优先级，请在外部按规则构建。
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct TraceState {
    entries: Vec<TraceStateEntry>,
}

impl TraceState {
    /// 创建空的 `TraceState`。
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// 基于已有条目构建 `TraceState`。
    pub fn from_entries(entries: Vec<TraceStateEntry>) -> Result<Self, TraceStateError> {
        let mut state = Self::new();
        for entry in entries {
            state.insert(entry)?;
        }
        Ok(state)
    }

    /// 插入或替换条目。
    pub fn insert(&mut self, entry: TraceStateEntry) -> Result<(), TraceStateError> {
        entry.validate()?;
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|current| current.key == entry.key)
        {
            *existing = entry;
        } else {
            self.entries.push(entry);
        }
        Ok(())
    }

    /// 删除指定键。
    pub fn remove(&mut self, key: &str) {
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            self.entries.remove(index);
        }
    }

    /// 迭代全部条目。
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &TraceStateEntry> {
        self.entries.iter()
    }

    /// 清空 `TraceState`。
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// 校验内部条目合法性。
    pub fn validate(&self) -> Result<(), TraceContextError> {
        for entry in &self.entries {
            entry
                .validate()
                .map_err(TraceContextError::InvalidTraceState)?;
        }
        Ok(())
    }
}

/// 单个 `tracestate` 条目。
///
/// # 契约说明（What）
/// - `key` 遵循 W3C 的语法限制；`value` 不可包含逗号或等号。
/// - **前置条件**：键、值长度均需小于等于 256 字节。
/// - **后置条件**：调用 [`Self::validate`] 成功后，可安全插入 [`TraceState`]。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TraceStateEntry {
    pub key: String,
    pub value: String,
}

impl TraceStateEntry {
    /// 创建新的 `tracestate` 条目。
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
        }
    }

    /// 校验条目是否符合 W3C 规范。
    pub fn validate(&self) -> Result<(), TraceStateError> {
        if self.key.is_empty() || self.key.len() > 256 {
            return Err(TraceStateError::InvalidKey);
        }
        if self.value.is_empty() || self.value.len() > 256 {
            return Err(TraceStateError::InvalidValue);
        }
        if self.value.contains(',') || self.value.contains('=') {
            return Err(TraceStateError::InvalidValue);
        }
        Ok(())
    }
}

/// `TraceState` 校验失败时的错误类型。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TraceStateError {
    InvalidKey,
    InvalidValue,
}

impl fmt::Display for TraceStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TraceStateError::InvalidKey => f.write_str("tracestate 键不合法"),
            TraceStateError::InvalidValue => f.write_str("tracestate 值不合法"),
        }
    }
}
