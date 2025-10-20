#![cfg(loom)]

use loom::{model, sync::Arc, thread};
use spark_core::AuditRecorder;
use spark_core::audit::{
    AuditActor, AuditChangeSet, AuditEntityRef, AuditEventV1, InMemoryAuditRecorder,
};
use spark_core::contract::{Budget, BudgetKind, Cancellation};

/// 构造最小化的审计事件载荷，确保在 Loom 模型中能够专注于互斥锁序列化逻辑。
///
/// # 教案级说明
/// - **意图 (Why)**：测试仅关注 `Mutex` 的并发行为，因此事件内容保持恒定，避免不相关字段干扰推理。
/// - **契约 (What)**：
///   - `sequence` 表示事件顺序；
///   - `prev_hash`/`curr_hash` 为哈希链值，允许我们控制链路是否连续；
///   - 返回值始终满足 `AuditEventV1` 的必需字段。
/// - **逻辑 (How)**：函数直接填充结构体字段，并将 `changes` 置为空集合，使事件的副作用仅体现在哈希链校验上。
/// - **注意事项 (Trade-offs)**：为了让不同线程写入相同事件而不会触发哈希冲突，默认使用相同的哈希值；若需要模拟冲突，可显式传入不同参数。
fn build_event(sequence: u64, prev_hash: &str, curr_hash: &str) -> AuditEventV1 {
    AuditEventV1 {
        event_id: format!("seq-{sequence}"),
        sequence,
        entity: AuditEntityRef {
            kind: "configuration.profile".into(),
            id: "demo".to_string(),
            labels: Vec::new(),
        },
        action: "apply".into(),
        state_prev_hash: prev_hash.to_string(),
        state_curr_hash: curr_hash.to_string(),
        actor: AuditActor {
            id: "tester".into(),
            display_name: None,
            tenant: None,
        },
        occurred_at: 0,
        tsa_evidence: None,
        changes: AuditChangeSet {
            created: Vec::new(),
            updated: Vec::new(),
            deleted: Vec::new(),
        },
    }
}

#[test]
fn cancellation_visibility_is_sequentially_consistent() {
    //
    // 教案级说明：该测试验证 `Cancellation` 在多线程下的内存可见性。
    // - **Why**：取消信号需要被其他协程及时感知，否则会导致超时/回滚机制失效。
    // - **How**：通过 Loom 穷举线程调度，观察 `cancel` 的释放语义是否能被 `is_cancelled` 的获取语义看见。
    // - **What**：若可见性正确，观察线程最终必然退出等待循环，且后续重复取消返回 `false`。
    // - **Trade-offs**：循环中使用 `thread::yield_now()` 限制忙等，确保 Loom 能探索足够的交错而不至于无限自旋。
    model(|| {
        let root = Cancellation::new();
        let worker = root.child();
        let observer = root.child();

        let canceler = thread::spawn(move || {
            assert!(worker.cancel(), "第一次取消必须成功");
        });

        let watcher = thread::spawn(move || {
            while !observer.is_cancelled() {
                thread::yield_now();
            }
        });

        canceler.join().expect("取消线程不应 panic");
        watcher.join().expect("观察线程不应 panic");
        assert!(root.is_cancelled(), "主线程应观察到取消标记");
        assert!(
            !root.cancel(),
            "重复取消应返回 false，验证 `compare_exchange` 的幂等语义"
        );
    });
}

#[test]
fn budget_concurrent_consume_and_refund_preserves_limits() {
    //
    // 教案级说明：验证预算控制器在并发消费与归还时不会出现下溢或超限。
    // - **Why**：预算常在多线程中共享，错误的原子序列可能导致资源被重复扣减或错误归还。
    // - **How**：两个线程分别尝试消耗不同额度，随后根据返回结果决定是否归还；
    //   Loom 将探索所有执行顺序，帮助我们确认最终剩余额度始终回到上限。
    // - **What**：测试结束后 `remaining()` 必须等于 `limit()`，表明没有漏记或重复记账。
    // - **Trade-offs**：线程间未强制顺序，允许出现 `Exhausted` 分支，用以覆盖竞争场景。
    model(|| {
        let budget = Arc::new(Budget::new(BudgetKind::Decode, 2));

        let fast_path = {
            let budget = Arc::clone(&budget);
            thread::spawn(move || {
                let decision = budget.try_consume(1);
                //
                // 教案级说明：若在交错中先行被另一线程抢占，也可能返回 `Exhausted`，
                // 因此仅在成功分配时执行归还操作，保持测试对调度顺序的鲁棒性。
                if decision.is_granted() {
                    budget.refund(1);
                }
            })
        };

        let greedy_path = {
            let budget = Arc::clone(&budget);
            thread::spawn(move || {
                let decision = budget.try_consume(2);
                if decision.is_granted() {
                    budget.refund(2);
                }
            })
        };

        fast_path.join().expect("快速路径线程不应 panic");
        greedy_path.join().expect("高消耗线程不应 panic");
        assert_eq!(
            budget.remaining(),
            budget.limit(),
            "无论调度顺序如何，剩余额度必须回到上限"
        );
    });
}

#[test]
fn audit_recorder_serialize_writes_without_deadlock() {
    //
    // 教案级说明：确保基于互斥锁的审计记录器在高竞争下保持顺序性与链路一致性。
    // - **Why**：若互斥实现有瑕疵，可能导致死锁或 `last_hash` 被交错更新，破坏哈希链。
    // - **How**：两个线程写入相同的事件快照，即便顺序反转也不会触发哈希冲突，
    //   因此我们只关注互斥是否正确串行化写入。
    // - **What**：完成后事件列表长度应为 2，且链表尾部哈希保持预期。
    // - **Trade-offs**：事件内容被人为固定为幂等值，牺牲了真实语义换取模型检查的确定性。
    model(|| {
        let recorder = Arc::new(InMemoryAuditRecorder::new());
        let event = build_event(1, "stable", "stable");

        let writer_a = {
            let recorder = Arc::clone(&recorder);
            let event = event.clone();
            thread::spawn(move || {
                recorder.record(event).expect("写入审计事件不应失败");
            })
        };

        let writer_b = {
            let recorder = Arc::clone(&recorder);
            let event = event.clone();
            thread::spawn(move || {
                recorder.record(event).expect("重复写入同一事件也应成功");
            })
        };

        writer_a.join().expect("线程 A 不应 panic");
        writer_b.join().expect("线程 B 不应 panic");

        let events = recorder.events();
        assert_eq!(events.len(), 2, "互斥锁应保证写入次数与线程数一致");
        assert!(
            events.iter().all(|evt| evt.state_curr_hash == "stable"),
            "最终链路哈希应保持稳定"
        );
    });
}
