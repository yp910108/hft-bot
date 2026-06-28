//! 时间抽象：策略与 engine 只依赖 [`Clock`] trait，不直接碰系统时钟。
//!
//! 时间在策略里无处不在：剩余时间 TTE、单步超时、冷却、熔断恢复迟滞。
//! 把它抽象成 trait，回测注入虚拟时钟（行情时间戳推进，可复现、可加速），
//! 实盘注入系统 wall-clock。同一套策略代码两边运行。
//!
//! 时刻用「自场开始起的毫秒数」表示（`Millis`），而非挂钟绝对时间——
//! 二元市场每场 15 分钟独立，相对场内时间既够用又天然可复现。

/// 自本场开始起经过的毫秒数。
pub type Millis = u64;

/// 时间源。给出「现在是本场的第几毫秒」。
pub trait Clock {
    /// 当前时刻（自场开始起的毫秒数）。
    fn now(&self) -> Millis;
}

/// 虚拟时钟：时间由外部显式推进，不依赖真实时间。回测用。
///
/// 回测把每个行情快照的时间戳灌进来，时间就「跳」到那一刻，
/// 使超时、冷却、TTE 全部可复现。
#[derive(Debug, Clone, Default)]
pub struct VirtualClock {
    now: Millis,
}

impl VirtualClock {
    /// 从 0 时刻起的虚拟时钟。
    pub fn new() -> Self {
        Self { now: 0 }
    }

    /// 直接设到某一时刻（行情时间戳驱动用）。只许前进，回退会 panic。
    pub fn set(&mut self, now: Millis) {
        assert!(now >= self.now, "虚拟时钟只能前进：{} → {}", self.now, now);
        self.now = now;
    }

    /// 前进指定毫秒数。
    pub fn advance(&mut self, delta: Millis) {
        self.now += delta;
    }
}

impl Clock for VirtualClock {
    fn now(&self) -> Millis {
        self.now
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_clock_starts_at_zero() {
        assert_eq!(VirtualClock::new().now(), 0);
    }

    #[test]
    fn set_jumps_to_timestamp() {
        let mut clock = VirtualClock::new();
        clock.set(5000);
        assert_eq!(clock.now(), 5000);
    }

    #[test]
    fn advance_adds_delta() {
        let mut clock = VirtualClock::new();
        clock.advance(1000);
        clock.advance(500);
        assert_eq!(clock.now(), 1500);
    }

    #[test]
    #[should_panic(expected = "只能前进")]
    fn set_backwards_panics() {
        let mut clock = VirtualClock::new();
        clock.set(5000);
        clock.set(4000);
    }
}
