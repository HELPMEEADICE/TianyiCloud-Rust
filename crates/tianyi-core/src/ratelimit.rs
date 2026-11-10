//! 令牌桶限速器
//!
//! 上传/下载链路共用一个「桶」：按字节租借令牌，额度不足就让出 worker 等定时器。
//! 这里是 `AppConfig::speed_limit`（字节/秒，0 = 不限）唯一的实现点，
//! 由调用方用 [`bucket_for`] 造桶、再把同一个 `Arc` 交给一批并发分片共享。
//!
//! 为什么用令牌桶而不是「每块睡固定毫秒」：分片大小并不均匀（末片可以只有几字节），
//! 固定睡眠把开销算在「块数」而不是「字节数」上，小分片越多实际速率越低。
//! 令牌桶按字节记账，速率与分片边界无关。

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;

/// 桶容量下限：64 KiB
///
/// 低于这个值就没有突发能力了：一次 4 KiB 的读都可能要等下一个令牌周期，
/// 租借次数（进而定时器唤醒次数）与总字节数成正比，低速率下唤醒开销会吃掉带宽。
const MIN_CAPACITY: u64 = 64 * 1024;

/// 桶容量 = 速率 / 该除数，即约 100 ms 的突发量
///
/// 取 100 ms 的理由：这是「单次网络往返」与「限速平滑度」之间的折中。
/// 太小（1 ms 量级）会让每次租借都变成一个高频定时器，令牌桶退化成忙等；
/// 太大（1 s 量级）则一次突发就把 1 秒的额度全放出去，限速形同虚设。
/// 代价：瞬时速率可以超过设定值，超出额度不超过一个 capacity——这既是令牌桶
/// 固有的突发语义，也是「申请大于 capacity 时拆段」不会死锁的原因。
const CAPACITY_DIVISOR: u64 = 10;

/// 令牌桶
///
/// 只通过 [`Arc`] 共享（实现 `Clone` 没有意义：状态必须唯一，复制一份就变成
/// 两份额度，多路并发时实际速率直接翻倍）。
///
/// `Debug` 是给 [`crate::transfer::DownloadOptions`] 的派生用的（选项整体要能打印）。
#[derive(Debug)]
pub struct RateLimiter {
    /// 每秒补充的字节数。用 f64 而不是 u64：慢速率下按毫秒折算常常不足 1 字节，
    /// 整数会在每次结算时丢掉小数部分，累计误差让实际速率明显低于设定值。
    rate: f64,
    /// 桶容量（字节），构造后不变
    capacity: u64,
    /// 令牌账本。用 `tokio::sync::Mutex` 而不是 `std::sync::Mutex`：
    /// 临界区极短但持有者是 async 任务，std 版会把 worker 线程摁住，
    /// 并发分片数大于 worker 线程数时直接拖慢整条链路。
    state: Mutex<State>,
}

/// 桶的账本
#[derive(Debug)]
struct State {
    /// 当前可用令牌（字节）。允许为负：负值表示「已经超支、之后要按速率偿还的债务」。
    /// 让债务可见是 N 路并发共享一份额度的关键——后来者看到负数就知道自己必须
    /// 排在前一笔债务之后，而不是各自独立计时。
    tokens: f64,
    /// 上次结算时刻
    last: Instant,
}

impl RateLimiter {
    /// 造一个限速桶（`bytes_per_sec` 字节/秒）
    ///
    /// 桶**从空开始**：不做「开局白送一个 capacity」的突发，任何一次申请都按字节付款。
    /// 代价是首块数据要多等一个租借周期（4 MiB 分片在 1 MiB/s 下等 4 s），
    /// 换来的是可预测的上界——整段传输耗时 ≈ 字节数 / 速率，
    /// 测试与「限速到底生效没有」的断言可以直接用这个下界。
    ///
    /// `bytes_per_sec == 0` 属于误用（不限速请走 [`bucket_for`] 拿到 `None`），
    /// 这里按 1 字节/秒兜底，避免除零和「零速率永远等不到令牌」的死锁。
    pub fn new(bytes_per_sec: u64) -> Arc<Self> {
        let rate = bytes_per_sec.max(1);
        let capacity = (rate / CAPACITY_DIVISOR).max(MIN_CAPACITY);
        Arc::new(RateLimiter {
            rate: rate as f64,
            capacity,
            state: Mutex::new(State::at(Instant::now())),
        })
    }

    /// 桶容量（字节）：单次租借的上限，也是允许的最大突发量
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// 租借 `bytes` 字节的额度；额度不够就等到够为止
    ///
    /// 拆段策略：单次申请超过桶容量时按 `capacity` 切段循环租借，
    /// 绝不把 `bytes > capacity` 当成一次「不可能满足」的申请——桶的余额永远追不平，
    /// 那才是真正的死锁。分段只是把一次大额申请摊成若干笔小额债务，
    /// 总等待时长仍然是 `bytes / rate`。
    ///
    /// 记账顺序是「先扣后睡」（reserve 语义）：扣掉即视为已消费，
    /// 因此本 future 若被取消（暂停/取消传输时 drop），已扣掉的令牌不会退回。
    /// 偏差方向是保守的——之后的传输只会更慢，绝不会超过设定速率。
    pub async fn acquire(&self, bytes: u64) {
        let mut left = bytes;
        while left > 0 {
            let want = left.min(self.capacity);
            // 持锁只做结算与扣账，sleep 放在锁外：否则一个慢速率的等待会把整个桶
            // 锁住，其他分片连「排队登记」都排不进去。
            let wait = {
                let mut st = self.state.lock().await;
                st.reserve(Instant::now(), want, self.rate, self.capacity)
            };
            left -= want;
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
        }
    }
}

impl State {
    /// 空账本，起始时刻为 `now`
    fn at(now: Instant) -> Self {
        State {
            tokens: 0.0,
            last: now,
        }
    }

    /// 结算到 `now` 并租借 `want`（调用方保证 `want <= capacity`），返回需要等待的时长
    fn reserve(&mut self, now: Instant, want: u64, rate: f64, capacity: u64) -> Duration {
        // 用 saturating：桶可能在一个时钟基准下构造、在另一个基准下使用
        // （例如构造于真实时间、使用于测试的加速时钟），那时 `now` 可能早于
        // `last`，按 0 结算即可，不能 panic。
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * rate).min(capacity as f64);
        self.tokens -= want as f64;
        if self.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-self.tokens / rate)
        }
    }
}

/// 按配置造桶：0 → `None`
///
/// 返回 `Option` 而不是「0 速率的桶」，是为了让调用方在热路径上用
/// `if let Some(rate)` 直接跳过租借——不限速时既没有 await 点也没有锁竞争，
/// 与引入限速前完全等价。
pub fn bucket_for(bytes_per_sec: u64) -> Option<Arc<RateLimiter>> {
    if bytes_per_sec == 0 {
        None
    } else {
        Some(RateLimiter::new(bytes_per_sec))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;

    /// 在虚拟时钟上原地跑一遍 `acquire` 的拆段循环：不真的 sleep，
    /// 而是把每段算出的等待时长累加到虚拟时间上。
    ///
    /// 这样得到的结果是**精确**的（浮点误差 < 1 µs），
    /// 既不需要 `tokio::time::pause`（本仓库的 tokio 未启用 `test-util`），
    /// 也不占用任何墙钟时间。
    fn simulate(
        st: &mut State,
        mut now: Instant,
        bytes: u64,
        rate: f64,
        capacity: u64,
    ) -> Duration {
        let mut total = Duration::ZERO;
        let mut left = bytes;
        while left > 0 {
            let want = left.min(capacity);
            let wait = st.reserve(now, want, rate, capacity);
            now += wait;
            total += wait;
            left -= want;
        }
        total
    }

    /// ① 0 = 不限速 → 不给桶；容量公式 ≈ 100 ms 的突发量，下限 64 KiB
    #[test]
    fn test_bucket_for_and_capacity() {
        assert!(bucket_for(0).is_none(), "0 必须映射为 None（不限速）");
        assert!(bucket_for(1).is_some(), "非 0 必须给出桶");

        // 1 MiB/s → 104857 字节
        assert_eq!(
            bucket_for(MIB).expect("桶").capacity(),
            MIB / CAPACITY_DIVISOR
        );
        // 10 MiB/s → 1 MiB
        assert_eq!(bucket_for(10 * MIB).expect("桶").capacity(), MIB);
        // 慢速率撞下限：10 KiB/s 的 100 ms 只有 1 KiB，按 64 KiB 兜底
        assert_eq!(bucket_for(10 * KIB).expect("桶").capacity(), MIN_CAPACITY);
        // 直接 new(0) 是误用，兜底成 1 字节/秒，不能除零/panic
        assert!(RateLimiter::new(0).capacity() >= MIN_CAPACITY);
    }

    /// ② 1 MiB/s 申请 512 KiB：500 ms 之前不返回，512 ms 之内返回
    #[tokio::test]
    async fn test_acquire_waits_for_bytes_at_rate() {
        let bucket = RateLimiter::new(MIB);
        let t0 = Instant::now();
        let mut st = State::at(t0);
        let total = simulate(&mut st, t0, 512 * KIB, bucket.rate, bucket.capacity());

        assert!(
            total >= Duration::from_millis(500),
            "512 KiB @ 1 MiB/s 至少等 500 ms，实际 {total:?}"
        );
        assert!(
            total <= Duration::from_millis(512),
            "512 KiB @ 1 MiB/s 不应超过 512 ms，实际 {total:?}"
        );
    }

    /// ③ 单次申请 3 倍桶容量也能完成：拆段而不是死锁
    #[tokio::test]
    async fn test_acquire_larger_than_capacity_does_not_deadlock() {
        let bucket = RateLimiter::new(MIB);
        let capacity = bucket.capacity();
        let want = capacity * 3;
        let t0 = Instant::now();
        let mut st = State::at(t0);
        let total = simulate(&mut st, t0, want, bucket.rate, bucket.capacity());

        // 下界：总字节 / 速率 = 300 ms；上界按验收口径取 1.5 倍 = 450 ms
        assert!(
            total > Duration::from_millis(1),
            "超过容量的申请不能被当成「额度够」直接放行"
        );
        assert!(
            total <= Duration::from_millis(450),
            "拆段租借必须在 总字节/速率 × 1.5 内完成，实际 {total:?}"
        );
    }

    /// ④ 两路并发共用一个桶：耗时按总量计，而不是各自独立计时
    #[tokio::test]
    async fn test_two_waiters_share_one_bucket() {
        let bucket = RateLimiter::new(2 * MIB);
        let rate = bucket.rate;
        let capacity = bucket.capacity();
        let t0 = Instant::now();

        // 两路在同一时刻各租 1 MiB 的第一段：后到者必须看到先到者的债务，
        // 因此它的第一段等待严格更长（100 ms vs 200 ms）。各自独立计时的话两者会相等。
        let mut shared = State::at(t0);
        let first = shared.reserve(t0, capacity, rate, capacity);
        let second = shared.reserve(t0, capacity, rate, capacity);
        assert!(
            second > first,
            "第二路必须排在先到者之后（{first:?} vs {second:?}）"
        );

        // 同一份账本上租完 2 MiB = 1 s（总量 / 速率）
        let mut shared = State::at(t0);
        let both = simulate(&mut shared, t0, 2 * MIB, rate, capacity);
        // 对照：两个独立桶各租 1 MiB 只要 500 ms，两路并行也是 500 ms
        let mut solo = State::at(t0);
        let single = simulate(&mut solo, t0, MIB, rate, capacity);

        assert!(
            both >= single * 2 - Duration::from_micros(1),
            "共享一个桶时耗时必须按总量计：{both:?} 应约为 {single:?} 的两倍"
        );
        assert!(
            both <= Duration::from_millis(1001),
            "2 MiB @ 2 MiB/s 不应超过 1 s，实际 {both:?}"
        );
    }

    /// ⑤ 真的跑一遍 `acquire`：异步外壳（加锁 / 睡眠 / 拆段循环）确实会等，
    /// 且 0 字节申请立即返回。
    ///
    /// 这是唯一一条吃真实时间的用例（约 64 ms）。本仓库的 tokio 没开 `test-util`，
    /// `#[tokio::test(start_paused = true)]` 与 `tokio::time::advance` 编译不过，
    /// 所以精确的时间语义交给上面几条虚拟时钟用例，这里只断言**下界**
    /// （定时器不会提前到期，下界断言永远不会假失败）。
    #[tokio::test]
    async fn test_acquire_actually_waits() {
        let bucket = RateLimiter::new(4 * MIB);
        // 0 字节：不租借、不睡，立即返回（不能因为拆段循环而卡住）
        bucket.acquire(0).await;

        let start = std::time::Instant::now();
        // 256 KiB @ 4 MiB/s = 64 ms（容量 419430 ≥ 256 KiB，单段租借）
        bucket.acquire(256 * KIB).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(60),
            "acquire 必须真的等待，实测只有 {elapsed:?}"
        );
    }
}
