//! ReadyState 状态机性质验证
//!
//! # 教案级注释概览
//!
//! - **核心目标 (Why)**：对 `ReadyState` 五态（`Ready`/`Busy`/`BudgetExhausted`/`RetryAfter`/`Pending`）进行形式化建模，
//!   验证任意“合法事件序列”在状态机中都可顺利驱动且不会落入不可达状态，同时确保一旦进入 `Pending`
//!   ，必然存在一次可观测的唤醒（`Wake`）。这些性质直接约束 `poll_ready()` 的契约，防止新实现遗漏唤醒或错用状态。
//! - **整体架构位置 (Why)**：测试位于 `spark-core/tests`，与 ReadyState 契约其他测试同级。模型层仅服务于属性验证，
//!   不回写生产代码，属于“影子规格 (Shadow Spec)”——其行为必须与文档《docs/state_machines.md》保持一致。
//! - **设计手法 (Why)**：使用 Proptest 构造“合法事件序列”，通过有限状态机 + 显式唤醒记录，分别断言：
//!   1. 转换总是存在（无 `unreachable`）；2. 每个 `Pending` 区间至少有一次合法唤醒源。
//!      该手法类似 *Model-Based Testing*，以纯 Rust 结构模拟框架契约。
//!
//! # 结构说明 (How)
//!
//! - `ReadyMachine`：影子状态机，包含当前节点、`Pending` 期望唤醒集合、访问信息等。
//! - `MachineEvent`：状态机输入事件，区分 `poll_ready()` 的返回 (`PollOutcome`) 与外部唤醒 (`Wake`)。
//! - `PendingExpectation`：`Pending` 区间的合同，记录允许的唤醒源与是否已被触发。
//! - `legal_sequences()`：利用 `SequenceBuilder` 根据随机控制流构造满足合同的事件序列。
//! - `prop_legal_sequences_have_no_unreachable_state`：性质 1，确保合法序列不会触发状态机错误。
//! - `prop_pending_intervals_have_visible_wake`：性质 2，确保每个 `Pending` 区间都观测到唤醒。
//!
//! # 合同与边界 (What)
//!
//! - **输入**：随机生成的 `Vec<MachineEvent>`，必须满足：
//!   - 每次 `Pending` 事件都至少允许一个唤醒源；
//!   - 从 `Pending` 返回非 `Pending` 状态前，必须出现合法唤醒；
//!   - 最终状态不可停留在“未唤醒的 Pending”。
//! - **输出/断言**：
//!   - 性质 1：`ReadyMachine` 对序列求值返回 `Ok(())`；访问到的状态全集仅包含定义域。
//!   - 性质 2：`ReadyMachine` 记录的所有 `Pending` 区间都标记为 `wake_observed=true`。
//! - **前置条件**：测试依赖 `docs/state_machines.md` 中的转换及唤醒源定义，不涉及真实 I/O。
//! - **后置条件**：一旦模型验证失败，将给出具体事件索引与错误说明，指导实现查缺补漏。
//!
//! # 设计考量 (Trade-offs)
//!
//! - 使用影子模型而非直接调用生产代码，避免框架未来重构导致测试过度耦合；代价是需人工维持模型与文档同步。
//! - `SequenceBuilder` 在生成序列时，会在结尾强制补齐唤醒 + 收敛到非 `Pending` 状态，以免序列收敛性影响性质验证。
//! - 唤醒源只列举 `IoReady`/`Timer`/`ConfigReload`，与文档保持一致；新增来源时需同时更新文档与本测试。
//! - 性质 1 与 2 分离，便于分别定位“状态机不可达”与“唤醒缺失”两类问题。
//!
//! # 风险与 TODO (Gotchas)
//!
//! - 若未来 ReadyState 引入新分支，需要补充 `ReadyLeafState` 与转换逻辑；否则生成器会遗漏新状态。
//! - 当 Pending 区间允许多个唤醒源时，`SequenceBuilder` 当前策略是固定使用集合中的第一个源；未来可扩展为随机选择，
//!   但需确保唤醒记录正确更新。
//! - TODO：若 ReadyState 状态机引入更多上下文（例如错误码），可考虑扩展模型记录以支持更细粒度的性质检查。

use std::collections::BTreeSet;

use proptest::prelude::*;

/// ReadyState 的影子节点。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ReadyNode {
    Ready,
    Busy,
    BudgetExhausted,
    RetryAfter,
    Pending,
}

/// ReadyState 的非 Pending 分支。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadyLeafState {
    Ready,
    Busy,
    BudgetExhausted,
    RetryAfter,
}

impl From<ReadyLeafState> for ReadyNode {
    fn from(state: ReadyLeafState) -> Self {
        match state {
            ReadyLeafState::Ready => ReadyNode::Ready,
            ReadyLeafState::Busy => ReadyNode::Busy,
            ReadyLeafState::BudgetExhausted => ReadyNode::BudgetExhausted,
            ReadyLeafState::RetryAfter => ReadyNode::RetryAfter,
        }
    }
}

/// 唤醒源，与文档中表格保持一致。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WakeSource {
    IoReady,
    Timer,
    ConfigReload,
}

impl WakeSource {
    /// 提供稳定的枚举集合，便于构造掩码。
    fn all() -> &'static [WakeSource] {
        &[
            WakeSource::IoReady,
            WakeSource::Timer,
            WakeSource::ConfigReload,
        ]
    }
}

/// `poll_ready()` 的返回结果。
#[derive(Clone, Debug, PartialEq, Eq)]
enum PollOutcome {
    State(ReadyLeafState),
    Pending { allowed_sources: Vec<WakeSource> },
}

/// 状态机事件。
#[derive(Clone, Debug, PartialEq, Eq)]
enum MachineEvent {
    Poll(PollOutcome),
    Wake(WakeSource),
}

/// Pending 区间合同。
#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingExpectation {
    allowed_sources: Vec<WakeSource>,
    wake_observed: bool,
}

impl PendingExpectation {
    fn new(allowed_sources: Vec<WakeSource>) -> Self {
        Self {
            allowed_sources,
            wake_observed: false,
        }
    }
}

/// Pending 区间历史记录，用于性质断言。
#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingRecord {
    allowed_sources: Vec<WakeSource>,
    wake_observed: bool,
}

/// 状态机错误信息。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
enum MachineError {
    #[error("pending expectation missing for wake")]
    MissingPending,
    #[error("wake source {0:?} not registered in current pending interval")]
    InvalidWake(WakeSource),
    #[error("pending interval finished without wake")]
    MissingWakeBeforeResolution,
    #[error("pending expectation must register at least one wake source")]
    EmptyWakeSet,
}

/// ReadyState 影子状态机。
#[derive(Debug)]
struct ReadyMachine {
    node: ReadyNode,
    pending: Option<PendingExpectation>,
    visited: BTreeSet<ReadyNode>,
    pending_history: Vec<PendingRecord>,
}

impl ReadyMachine {
    fn new() -> Self {
        let mut machine = Self {
            node: ReadyNode::Ready,
            pending: None,
            visited: BTreeSet::new(),
            pending_history: Vec::new(),
        };
        machine.visited.insert(ReadyNode::Ready);
        machine
    }

    /// 对单个事件求值。
    fn apply(&mut self, event: &MachineEvent) -> Result<(), MachineError> {
        match event {
            MachineEvent::Poll(PollOutcome::State(state)) => {
                let next_node = ReadyNode::from(*state);
                if let Some(pending) = self.pending.take() {
                    if !pending.wake_observed {
                        return Err(MachineError::MissingWakeBeforeResolution);
                    }
                    self.pending_history.push(PendingRecord {
                        allowed_sources: pending.allowed_sources,
                        wake_observed: true,
                    });
                }
                self.node = next_node;
                self.visited.insert(next_node);
                Ok(())
            }
            MachineEvent::Poll(PollOutcome::Pending { allowed_sources }) => {
                if allowed_sources.is_empty() {
                    return Err(MachineError::EmptyWakeSet);
                }
                self.visited.insert(ReadyNode::Pending);
                match self.pending.as_mut() {
                    Some(pending) => {
                        pending.allowed_sources = allowed_sources.clone();
                        Ok(())
                    }
                    None => {
                        self.pending = Some(PendingExpectation::new(allowed_sources.clone()));
                        self.node = ReadyNode::Pending;
                        Ok(())
                    }
                }
            }
            MachineEvent::Wake(source) => {
                if let Some(pending) = self.pending.as_mut() {
                    if pending.allowed_sources.contains(source) {
                        pending.wake_observed = true;
                        Ok(())
                    } else {
                        Err(MachineError::InvalidWake(*source))
                    }
                } else {
                    Err(MachineError::MissingPending)
                }
            }
        }
    }
}

/// 构造合法事件序列的生成器。
fn legal_sequences() -> impl Strategy<Value = Vec<MachineEvent>> {
    prop::collection::vec(any::<u8>(), 1..=64).prop_map(|controls| {
        let mut builder = SequenceBuilder::new();
        for control in controls {
            builder.push(control);
        }
        builder.finish()
    })
}

/// 仅包含至少一个 Pending 的序列，用于 Pending 唤醒性质验证。
fn legal_sequences_with_pending() -> impl Strategy<Value = Vec<MachineEvent>> {
    legal_sequences().prop_filter("sequence must contain pending", |events| {
        events
            .iter()
            .any(|event| matches!(event, MachineEvent::Poll(PollOutcome::Pending { .. })))
    })
}

/// 构造事件序列的辅助状态。
struct SequenceBuilder {
    events: Vec<MachineEvent>,
    node: ReadyNode,
    pending: Option<PendingExpectation>,
}

impl SequenceBuilder {
    fn new() -> Self {
        Self {
            events: Vec::new(),
            node: ReadyNode::Ready,
            pending: None,
        }
    }

    fn push(&mut self, control: u8) {
        match self.node {
            ReadyNode::Pending => self.drive_pending(control),
            _ => self.drive_non_pending(control),
        }
    }

    fn finish(mut self) -> Vec<MachineEvent> {
        if let Some(mut pending) = self.pending.take() {
            if !pending.wake_observed {
                let source = pending.allowed_sources[0];
                self.events.push(MachineEvent::Wake(source));
                pending.wake_observed = true;
            }
            self.events.push(MachineEvent::Poll(PollOutcome::State(
                ReadyLeafState::Ready,
            )));
            self.node = ReadyNode::Ready;
        }
        self.events
    }

    fn drive_non_pending(&mut self, control: u8) {
        let branch = control % 5;
        let leaf = match branch {
            0 => ReadyLeafState::Ready,
            1 => ReadyLeafState::Busy,
            2 => ReadyLeafState::BudgetExhausted,
            3 => ReadyLeafState::RetryAfter,
            _ => {
                let allowed = Self::select_sources(control / 5);
                self.events.push(MachineEvent::Poll(PollOutcome::Pending {
                    allowed_sources: allowed.clone(),
                }));
                self.pending = Some(PendingExpectation::new(allowed));
                self.node = ReadyNode::Pending;
                return;
            }
        };
        self.events
            .push(MachineEvent::Poll(PollOutcome::State(leaf)));
        self.node = ReadyNode::from(leaf);
    }

    fn drive_pending(&mut self, control: u8) {
        let pending = self.pending.as_mut().expect("pending state must exist");
        if !pending.wake_observed {
            if control % 3 == 0 {
                let index = (control as usize / 3) % pending.allowed_sources.len();
                let source = pending.allowed_sources[index];
                self.events.push(MachineEvent::Wake(source));
                pending.wake_observed = true;
            } else {
                let allowed = Self::select_sources(control);
                self.events.push(MachineEvent::Poll(PollOutcome::Pending {
                    allowed_sources: allowed.clone(),
                }));
                pending.allowed_sources = allowed;
            }
        } else {
            let leaf_selector = (control % 4) as usize;
            let leaf = [
                ReadyLeafState::Ready,
                ReadyLeafState::Busy,
                ReadyLeafState::BudgetExhausted,
                ReadyLeafState::RetryAfter,
            ][leaf_selector];
            self.events
                .push(MachineEvent::Poll(PollOutcome::State(leaf)));
            self.node = ReadyNode::from(leaf);
            self.pending = None;
        }
    }

    fn select_sources(seed: u8) -> Vec<WakeSource> {
        let mut mask = seed % 8;
        if mask == 0 {
            mask = 1;
        }
        let mut sources = Vec::new();
        for (bit, source) in WakeSource::all().iter().enumerate() {
            if (mask >> bit) & 1 == 1 {
                sources.push(*source);
            }
        }
        sources
    }
}

proptest! {
    #[test]
    fn prop_legal_sequences_have_no_unreachable_state(events in legal_sequences()) {
        let mut machine = ReadyMachine::new();
        for event in &events {
            prop_assert_eq!(machine.apply(event), Ok(()));
        }
        for node in &machine.visited {
            prop_assert!(matches!(node, ReadyNode::Ready | ReadyNode::Busy | ReadyNode::BudgetExhausted | ReadyNode::RetryAfter | ReadyNode::Pending));
        }
    }

    #[test]
    fn prop_pending_intervals_have_visible_wake(events in legal_sequences_with_pending()) {
        let mut machine = ReadyMachine::new();
        for event in &events {
            prop_assert_eq!(machine.apply(event), Ok(()));
        }
        prop_assert!(!machine.pending_history.is_empty());
        for record in &machine.pending_history {
            prop_assert!(record.wake_observed);
            prop_assert!(!record.allowed_sources.is_empty());
        }
    }
}
