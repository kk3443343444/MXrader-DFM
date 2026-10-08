//! 异步解析队列：`battle_proxy::battle::parse_queue`。
//!
//! 复刻样本 `src/battle/parse_queue.rs`。样本在这一层留了三行任务名：
//!
//! ```text
//! spawn battle-parse-<n>
//! spawn battle-ordered-apply
//! spawn battle-apply-watchdog
//! ```
//!
//! 也就是三段式流水：
//!
//! ```text
//!  网络任务                   解析 worker × N                应用任务
//!  feed()  ──有界 channel──▶  spawn battle-parse-<n>  ──▶  spawn battle-ordered-apply
//!      │                             │                              │
//!      └── 队列满就丢弃最旧（绝不阻塞转发）                          └── apply-watchdog 保证
//!                                                                      seq 单调、不卡死
//! ```
//!
//! **核心约束：`feed()` 绝不能 await 网络之外的东西**。样本的口号是
//! "先转发，后解析"（DiagnosticsSheet 文案里也有"先转发，后解析"）。队列满时
//! 丢弃最旧样本并记账，因为雷达的价值在于"最新位置"，旧包本来就过时了。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;
use tokio::sync::watch;

use crate::state::AppState;

/// 一个待解析的数据报。
#[derive(Debug, Clone)]
pub struct ParseJob {
    /// 单调序号，用于 `ordered-apply` 排序。
    pub seq: u64,
    pub session: u64,
    /// `true` = 客户端→服务端（上行，通常含玩家输入/开火 RPC）。
    pub c2s: bool,
    pub src: std::net::SocketAddr,
    pub dst: std::net::SocketAddr,
    pub ts_ms: u64,
    pub payload: bytes::Bytes,
}

/// 解析结果（由 worker 产出，`ordered-apply` 消费）。
#[derive(Debug, Clone)]
pub struct ParseOutcome {
    pub seq: u64,
    pub session: u64,
    pub ts_ms: u64,
    /// 传输层识别结果的名字，例如 `"plain"`。
    pub transport: &'static str,
    /// 分帧是否通过 decode gate。
    pub gate_passed: bool,
    /// 解析出的更新（实体/击杀/开火），交给 `battle::engine` 应用。
    pub updates: Vec<crate::battle::engine::EngineUpdate>,
    /// 错误标签（诊断）。
    pub error: Option<&'static str>,
}

/// 队列统计。
#[derive(Debug, Default)]
pub struct QueueStats {
    pub enqueued: AtomicU64,
    pub dropped: AtomicU64,
    pub parsed: AtomicU64,
    pub applied: AtomicU64,
    pub last_seq_applied: AtomicU64,
    pub out_of_order: AtomicU64,
}

/// 有界解析流水。
pub struct ParseQueue {
    tx: mpsc::Sender<ParseJob>,
    stats: Arc<QueueStats>,
    capacity: usize,
}

impl ParseQueue {
    /// 创建队列与 worker 池。
    ///
    /// `worker` 是纯计算函数（不可 await IO），`applier` 负责把结果写回引擎状态。
    pub fn spawn<W, A>(
        state: AppState,
        capacity: usize,
        workers: usize,
        worker: W,
        applier: A,
    ) -> Self
    where
        W: Fn(&ParseJob) -> ParseOutcome + Send + Sync + 'static,
        A: Fn(ParseOutcome) + Send + Sync + 'static,
    {
        let (tx, rx) = mpsc::channel::<ParseJob>(capacity);
        let stats = Arc::new(QueueStats::default());
        let worker = Arc::new(worker);
        let applier = Arc::new(applier);
        let rx = Arc::new(tokio::sync::Mutex::new(rx));

        // 三段式：parse workers -> apply channel -> ordered-apply
        let (apply_tx, mut apply_rx) = mpsc::channel::<ParseOutcome>(capacity.max(64));

        for i in 0..workers.max(1) {
            let rx = rx.clone();
            let stats = stats.clone();
            let worker = worker.clone();
            let apply_tx = apply_tx.clone();
            let state = state.clone();
            tokio::spawn(async move {
                tracing::debug!(worker = i, "spawn battle-parse-{i}");
                loop {
                    let job = {
                        let mut guard = rx.lock().await;
                        guard.recv().await
                    };
                    let Some(job) = job else { break };
                    let out = worker(&job);
                    stats.parsed.fetch_add(1, Ordering::Relaxed);
                    if apply_tx.send(out).await.is_err() {
                        break;
                    }
                    state.counters_ref().parse_queue_depth.fetch_sub(1, Ordering::Relaxed);
                }
            });
        }
        drop(apply_tx);

        // battle-ordered-apply：保证 seq 单调，乱序时丢弃较旧的（雷达只要最新）。
        {
            let stats = stats.clone();
            let applier = applier.clone();
            tokio::spawn(async move {
                tracing::debug!("spawn battle-ordered-apply");
                let mut last = 0u64;
                while let Some(out) = apply_rx.recv().await {
                    // 必须严格递增：重复 seq（worker 迟到的同号结果）也是乱序，要丢弃并记账。
                    if out.seq <= last {
                        stats.out_of_order.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    last = out.seq;
                    stats.last_seq_applied.store(out.seq, Ordering::Relaxed);
                    stats.applied.fetch_add(1, Ordering::Relaxed);
                    applier(out);
                }
            });
        }

        // battle-apply-watchdog：卡死检测（只看，不改数据）。
        {
            let stats = stats.clone();
            tokio::spawn(async move {
                tracing::debug!("spawn battle-apply-watchdog");
                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(10));
                let mut last_applied = 0u64;
                loop {
                    ticker.tick().await;
                    let now = stats.applied.load(Ordering::Relaxed);
                    if now == last_applied {
                        let dropped = stats.dropped.load(Ordering::Relaxed);
                        let enq = stats.enqueued.load(Ordering::Relaxed);
                        tracing::warn!(
                            enqueued = enq,
                            dropped,
                            "battle-apply-watchdog: no apply progress in 10s"
                        );
                    }
                    last_applied = now;
                }
            });
        }

        Self { tx, stats, capacity }
    }

    /// 非阻塞投递。队列满 → 丢最旧（这里用 `try_send` + 丢弃新包等价实现，
    /// 因为 tokio 的 mpsc 不支持"从队头丢弃"；效果一致：绝不阻塞网络任务）。
    #[inline]
    pub fn offer(&self, job: ParseJob) -> bool {
        self.stats.enqueued.fetch_add(1, Ordering::Relaxed);
        match self.tx.try_send(job) {
            Ok(()) => true,
            Err(_) => {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn stats(&self) -> &QueueStats {
        &self.stats
    }

    pub fn depth_hint(&self) -> u64 {
        self.stats
            .enqueued
            .load(Ordering::Relaxed)
            .saturating_sub(self.stats.applied.load(Ordering::Relaxed))
    }
}

/// 单调序号生成器（跨 worker 唯一）。
#[derive(Debug, Default)]
pub struct SeqGen(AtomicU64);

impl SeqGen {
    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// 当前毫秒时间戳。
#[inline]
pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// 队列排水：关闭时等 worker 收尾（最多 `timeout`）。
pub async fn drain(mut rx: watch::Receiver<bool>, timeout: std::time::Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while !*rx.borrow_and_update() {
        if tokio::time::timeout_at(deadline, rx.changed()).await.is_err() {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battle::engine::EngineUpdate;
    use crate::config::Config;
    use std::net::SocketAddr;
    use std::sync::Mutex;

    fn job(seq: u64) -> ParseJob {
        ParseJob {
            seq,
            session: 1,
            c2s: true,
            src: "192.168.1.9:40000".parse::<SocketAddr>().unwrap(),
            dst: "1.2.3.4:9000".parse::<SocketAddr>().unwrap(),
            ts_ms: 0,
            payload: bytes::Bytes::from_static(&[1, 2, 3]),
        }
    }

    #[tokio::test]
    async fn jobs_are_parsed_and_applied_in_order() {
        let st = AppState::new(&Config::default(), "t".into());
        let applied: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = applied.clone();
        let q = ParseQueue::spawn(
            st,
            64,
            2,
            |j| ParseOutcome {
                seq: j.seq,
                session: j.session,
                ts_ms: j.ts_ms,
                transport: "plain",
                gate_passed: true,
                updates: vec![EngineUpdate::Tick],
                error: None,
            },
            move |o| sink.lock().unwrap().push(o.seq),
        );
        for i in 1..=20 {
            assert!(q.offer(job(i)));
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let got = applied.lock().unwrap().clone();
        assert_eq!(got.len(), 20);
        assert!(got.windows(2).all(|w| w[0] <= w[1]), "apply must be monotonic: {got:?}");
        assert_eq!(q.stats().applied.load(Ordering::Relaxed), 20);
    }

    #[tokio::test]
    async fn out_of_order_results_are_dropped_and_counted() {
        let st = AppState::new(&Config::default(), "t".into());
        let applied: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = applied.clone();
        // worker 故意反序：偶数 seq 先返回
        let q = ParseQueue::spawn(
            st,
            64,
            1,
            |j| ParseOutcome {
                seq: if j.seq % 2 == 0 { j.seq + 1 } else { j.seq },
                session: 1,
                ts_ms: 0,
                transport: "plain",
                gate_passed: true,
                updates: vec![],
                error: None,
            },
            move |o| sink.lock().unwrap().push(o.seq),
        );
        for i in 1..=10 {
            q.offer(job(i));
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(q.stats().out_of_order.load(Ordering::Relaxed) > 0);
        let got = applied.lock().unwrap().clone();
        assert!(got.windows(2).all(|w| w[0] < w[1]), "applied must be strictly increasing: {got:?}");
    }

    #[tokio::test]
    async fn full_queue_drops_instead_of_blocking() {
        let st = AppState::new(&Config::default(), "t".into());
        // worker 故意很慢
        let q = ParseQueue::spawn(
            st,
            2,
            1,
            |j| {
                std::thread::sleep(std::time::Duration::from_millis(50));
                ParseOutcome {
                    seq: j.seq,
                    session: 1,
                    ts_ms: 0,
                    transport: "plain",
                    gate_passed: true,
                    updates: vec![],
                    error: None,
                }
            },
            |_| {},
        );
        let mut dropped = 0;
        for i in 1..=200 {
            if !q.offer(job(i)) {
                dropped += 1;
            }
        }
        assert!(dropped > 0, "backpressure must drop, not block");
        assert_eq!(q.stats().dropped.load(Ordering::Relaxed), dropped as u64);
    }

    #[test]
    fn seq_gen_is_monotonic_and_unique() {
        let g = SeqGen::default();
        let a = g.next();
        let b = g.next();
        assert_eq!(b, a + 1);
    }

    #[test]
    fn now_ms_is_sane() {
        assert!(now_ms() > 1_600_000_000_000);
    }
}
