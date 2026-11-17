//! 速度限制：全局唯一的桶缓存 + 设置页分段的档位映射
//!
//! `AppConfig::speed_limit`（字节/秒，0 = 不限）是唯一事实来源，桶本身由
//! `tianyi_core::ratelimit::bucket_for` 造。这里是「谁来造桶」这件事的唯一实现点。
//!
//! # 为什么必须是单例
//!
//! 桶的额度是「每秒 N 字节」，N 来自配置。若每个任务各造一个桶，两个并发任务的额度
//! 就是各一份、合起来 2N：用户把速度限制设成 1 MB/s，实际能跑到 2 MB/s，限速形同
//! 虚设，而界面上还写着 1 MB/s。所以按限速值缓存桶，传输侧每次启动任务只来这里
//! 「取当前值对应的桶」，同一个值永远拿到同一个 `Arc`。
//!
//! 代价：改限速值不会让在跑的任务变快/变慢——它们手里还握着启动时那个 `Arc`，
//! 旧桶要等最后一个持有者 drop 才失效。这是刻意的（见 `Controller::set_speed_limit`）：
//! 中途改速率会让已建立的分片会话失去对应关系，与「只对新任务生效」的口径一致。

use super::*;
use tianyi_core::file;
use tianyi_core::ratelimit::{bucket_for, RateLimiter};

/// 速度限制的预设档位（字节/秒），与设置页分段的顺序一一对应
///
/// 1 MB = 1048576 字节：必须与 `tianyi_core::file::format_size` 的 1024 进制一致。
/// 若按 1000 进制取整，用户选了「1 MB/s」、行内小字却会显示「当前 1.05 MB/s」
/// （1048576 按 1000 进制折算是 1.05 M），同一份配置在两个地方写成两个数字。
pub(super) const SPEED_PRESETS: [u64; 5] = [
    0,
    1024 * 1024,
    5 * 1024 * 1024,
    10 * 1024 * 1024,
    20 * 1024 * 1024,
];

/// 分段档位数（= `SPEED_PRESETS.len()`）
///
/// 单独写出来只为一件事：`clamp(0, N - 1)` 里不出现裸字面量，加档位时只改数组。
pub(super) const SPEED_PRESET_COUNT: i32 = SPEED_PRESETS.len() as i32;

/// 限速值 → 设置页分段的索引；非预设值返回 `None`
///
/// 返回 `None` 而不是「最接近的预设」：分段控件必须能表达「一个都不选中」。
/// 若退到最接近的预设，用户手改过 `config.json`（比如 3 MB/s）时界面会高亮
/// 「5 MB/s」，而他真正生效的是 3 MB/s——界面上的选中项与实际设定不符，
/// 比「没有选中 + 一行小字写明真实值」更容易误导人。
pub(super) fn index_of_limit(bytes: u64) -> Option<i32> {
    SPEED_PRESETS
        .iter()
        .position(|&preset| preset == bytes)
        .map(|idx| idx as i32)
}

/// 分段索引 → 限速值（越界索引钳到 `0..=4`）
///
/// 钳制放在这里而不是调用方：设置页的分段控件只能产出 0..=4，但将来的快捷键 /
/// 命令行参数可能传来脏值，一个 `clamp` 就能保证「写入配置的值一定是合法档位」。
pub(super) fn limit_of_index(idx: i32) -> u64 {
    SPEED_PRESETS[idx.clamp(0, SPEED_PRESET_COUNT - 1) as usize]
}

/// 限速值 → (分段索引, 行内小字文案)
///
/// 预设档给 `(0..=4, "")`：高亮的那一段自己就说明了当前设定，不必再写一行字。
/// 非预设档给 `(-1, "当前 3.00 MB/s（自定义）")`：`FluentSegmented` 只在
/// `current == idx` 时点亮某一段，-1 就是「五段全不亮」，同时那行小字把真实值
/// 摆在旁边——两者合起来才是「不许显示与实际不符的选中项」。
pub(super) fn speed_limit_ui(limit: u64) -> (i32, String) {
    match index_of_limit(limit) {
        Some(idx) => (idx, String::new()),
        // 文案走词表：这条是 Rust 侧拼出来的，`@tr` 绑不住（切语时要由控制器重推）
        None => (
            -1,
            tf(
                "state.speed_limit_custom",
                &[("value", &file::format_speed(limit))],
            ),
        ),
    }
}

/// 限速桶缓存：同一个限速值只造一个桶
pub(super) struct SpeedLimitCache {
    /// 当前缓存的限速值（0 = 不限速）
    limit: u64,
    /// `limit` 对应的桶；`limit == 0` 时恒为 `None`
    bucket: Option<Arc<RateLimiter>>,
}

impl SpeedLimitCache {
    /// 空缓存：初值 0（= 配置默认的不限速）
    pub(super) fn new() -> Self {
        Self {
            limit: 0,
            bucket: None,
        }
    }

    /// 取 `limit` 对应的桶：同值复用同一个 `Arc`，值变了就重建
    ///
    /// 0 会把缓存清空并返回 `None`（不限速时热路径上连一次锁租借都不该发生）。
    /// 清空也是「回到限速后一定拿到新桶」的原因：旧桶可能已经被不限速那一段时间
    /// 里的调用方 drop 掉，留着它的 `Arc` 只会让令牌账本里躺着一份过期债务。
    pub(super) fn for_limit(&mut self, limit: u64) -> Option<Arc<RateLimiter>> {
        if limit == 0 {
            self.limit = 0;
            self.bucket = None;
            return None;
        }
        if limit == self.limit {
            if let Some(bucket) = &self.bucket {
                // 关键的一行：克隆**同一个** Arc，而不是再造一个桶
                return Some(Arc::clone(bucket));
            }
        }
        let bucket = bucket_for(limit);
        self.limit = limit;
        self.bucket = bucket.clone();
        bucket
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    /// ① 同一个限速值两次取桶 → 必须是**同一个** `Arc`
    ///
    /// 这条断言就是「全局单一桶」的守卫：只要 `for_limit` 退化成
    /// `bucket_for(limit)`（每次新建），两个并发任务就各持一个桶、合起来能跑到 2N。
    #[test]
    fn speed_limit_cache_reuses_one_bucket_per_value() {
        let mut cache = SpeedLimitCache::new();
        let first = cache.for_limit(MIB).expect("非 0 必须给出桶");
        let second = cache.for_limit(MIB).expect("非 0 必须给出桶");

        assert!(
            Arc::ptr_eq(&first, &second),
            "同一个限速值两次取桶必须是同一个 Arc —— 各造一个桶时，设 1 MB/s 实际能跑到 2 MB/s"
        );
        // 顺带确认拿到的确实是「按这个速率造出来的桶」，而不是别的什么的克隆
        assert_eq!(second.capacity(), first.capacity());
        assert_eq!(second.capacity(), MIB / 10, "容量 = 速率的 100 ms 突发量");
    }

    /// ② 限速值变了 → 换桶（绝不就地改速率）
    ///
    /// 桶的 `rate` 是构造后不变的：改值只能新建。旧桶仍被在跑的任务持有，
    /// 所以「换桶」对它们没有影响——这正是「只对新任务生效」的实现方式。
    #[test]
    fn speed_limit_cache_rebuilds_when_value_changes() {
        let mut cache = SpeedLimitCache::new();
        let slow = cache.for_limit(MIB).expect("桶");
        let fast = cache.for_limit(2 * MIB).expect("桶");

        assert!(!Arc::ptr_eq(&slow, &fast), "限速值变了必须换成新桶");
        assert_eq!(fast.capacity(), 2 * MIB / 10, "新桶必须按新速率造");

        // 换回原值也不许复用最初那个桶（它已经被换下去了，缓存里只留最新的一份）
        let slow_again = cache.for_limit(MIB).expect("桶");
        assert!(!Arc::ptr_eq(&slow, &slow_again));
        assert!(!Arc::ptr_eq(&fast, &cache.for_limit(MIB).expect("桶")));
    }

    /// ③ 0 = 不限速 → `None`，并且缓存被清空
    #[test]
    fn speed_limit_cache_clears_on_zero() {
        let mut cache = SpeedLimitCache::new();
        let first = cache.for_limit(MIB).expect("桶");

        assert!(cache.for_limit(0).is_none(), "0 必须映射成 None（不限速）");
        assert_eq!(cache.limit, 0, "清空后缓存的限速值必须归零");
        assert!(cache.bucket.is_none(), "清空后不许留着旧桶");

        // 从「不限速」再回到限速：拿到的是新桶，不是被清掉的那一份
        let second = cache.for_limit(MIB).expect("桶");
        assert!(!Arc::ptr_eq(&first, &second), "回到限速后必须是新桶");
    }

    /// ④ 连续 0（含启动初值）→ 一直 `None`，不 panic
    ///
    /// 启动时配置默认就是 0，每个新任务都会来问一次；这条路径上任何 panic 都会
    /// 变成「不限速时一开传输就崩」。
    #[test]
    fn speed_limit_cache_tolerates_repeated_zero() {
        let mut cache = SpeedLimitCache::new();
        for round in 0..3 {
            assert!(
                cache.for_limit(0).is_none(),
                "第 {round} 次 0 必须给出 None"
            );
        }
        assert_eq!(cache.limit, 0);
        assert!(cache.bucket.is_none());
    }

    /// ⑤ 索引 ↔ 字节/秒：预设值一一对应，非预设值必须落到「无选中」
    #[test]
    fn speed_limit_index_maps_presets_and_rejects_custom() {
        assert_eq!(index_of_limit(0), Some(0), "不限速是第一段");
        assert_eq!(index_of_limit(MIB), Some(1), "1 MB/s = 1048576 字节/秒");
        assert_eq!(index_of_limit(5 * MIB), Some(2));
        assert_eq!(index_of_limit(10 * MIB), Some(3));
        assert_eq!(index_of_limit(20 * MIB), Some(4));

        // 手改过 config.json 的非预设值：界面必须承认「没有选中任何一段」
        assert_eq!(index_of_limit(3 * MIB), None, "3 MB/s 不是预设");
        assert_eq!(
            index_of_limit(1_000_000),
            None,
            "按 1000 进制的 1 MB 不是预设：预设只认 1048576"
        );

        // 反向映射：越界索引钳到两端，绝不 panic 也不越界读数组
        assert_eq!(limit_of_index(0), 0);
        assert_eq!(limit_of_index(1), MIB);
        assert_eq!(limit_of_index(-7), 0, "负数钳到下界");
        assert_eq!(limit_of_index(99), 20 * MIB, "越上界钳到最后一档");

        // 两个方向不许分叉：每个预设都能映射回自己的索引
        for (idx, &limit) in SPEED_PRESETS.iter().enumerate() {
            assert_eq!(index_of_limit(limit), Some(idx as i32));
            assert_eq!(limit_of_index(idx as i32), limit);
        }
    }
}
