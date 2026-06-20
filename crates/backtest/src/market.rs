//! 合成行情生成器：参数化生成 BTC 价格随机游走，并映射为二元市场盘口序列。
//!
//! 通过可控的波动率与趋势漂移，可构造「纯震荡」「单边趋势」等极端场景来验证策略 edge。
//! 随机数采用可注入种子的确定性生成器，保证回测可复现。

use domain::market::{BookTop, MarketSnapshot};
use domain::types::{Price, Side};
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use rust_decimal_macros::dec;

/// 确定性线性同余随机数生成器（LCG）。
///
/// 仅用于回测生成可复现的伪随机序列，不用于任何安全敏感场景。
/// 参数取自 Numerical Recipes 的常用常量。
#[derive(Debug, Clone)]
pub struct Lcg {
    state: u64,
}

impl Lcg {
    /// 以指定种子创建生成器。
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// 推进并返回下一个 [0, 1) 区间的浮点数。
    fn next_unit(&mut self) -> f64 {
        // LCG 递推：state = state * a + c (mod 2^64)。
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        // 取高 53 位映射到 [0, 1)。
        (self.state >> 11) as f64 / (1u64 << 53) as f64
    }

    /// 返回 [-1, 1) 区间的对称随机步长。
    fn next_signed(&mut self) -> f64 {
        self.next_unit() * 2.0 - 1.0
    }
}

/// 合成行情配置。
#[derive(Debug, Clone, Copy)]
pub struct SyntheticMarketConfig {
    /// 起始 BTC 价格。
    pub start_price: f64,
    /// 行情步数（每步一个盘口快照）。
    pub steps: usize,
    /// 每步波动幅度（价格随机游走的步长尺度）。
    pub volatility: f64,
    /// 每步趋势漂移（正数上行、负数下行、0 为纯震荡）。
    pub drift: f64,
    /// 二元盘口的买卖价差（half-spread，挂在公允价两侧）。
    pub half_spread: Decimal,
    /// 价格→概率映射的灵敏度（logistic 斜率）：越大则同样的 BTC 价格偏离引起越剧烈的
    /// 概率变化，对应二元市场临近结算时对标的更敏感的特性。
    pub sensitivity: f64,
    /// 随机数种子。
    pub seed: u64,
}

impl Default for SyntheticMarketConfig {
    /// 默认：起始价 100000，200 步，纯震荡（drift 0），价差 0.01，灵敏度 80。
    ///
    /// 灵敏度 80 下，BTC 涨 1%（相对偏离 0.01）使 Up 概率约 0.5→0.69，盘口随波动明显移动。
    fn default() -> Self {
        Self {
            start_price: 100_000.0,
            steps: 200,
            volatility: 50.0,
            drift: 0.0,
            half_spread: dec!(0.01),
            sensitivity: 80.0,
            seed: 1,
        }
    }
}

/// 一场合成行情：盘口快照序列 + 最终交割胜出方。
#[derive(Debug, Clone)]
pub struct SyntheticMarket {
    /// 按时间顺序的盘口快照序列。
    pub snapshots: Vec<MarketSnapshot>,
    /// 交割时的胜出方（由末价相对起始价的涨跌决定：涨则 Up 胜，否则 Down 胜）。
    pub winner: Side,
}

/// 依据配置生成一场合成行情。
pub fn generate(config: &SyntheticMarketConfig) -> SyntheticMarket {
    let mut rng = Lcg::new(config.seed);
    let mut price = config.start_price;
    let mut snapshots = Vec::with_capacity(config.steps);

    for _ in 0..config.steps {
        // 随机游走：价格 += 趋势漂移 + 波动 × 对称随机步长。
        price += config.drift + config.volatility * rng.next_signed();
        snapshots.push(snapshot_from_price(
            price,
            config.start_price,
            config.half_spread,
            config.sensitivity,
        ));
    }

    // 末价相对起始价的涨跌决定交割胜出方。
    let winner = if price >= config.start_price {
        Side::Up
    } else {
        Side::Down
    };

    SyntheticMarket { snapshots, winner }
}

/// 把一个 BTC 价格映射为二元市场盘口快照。
///
/// 公允价用「当前价相对起始价的偏离」经有界压缩映射到 (0, 1)：价格高于起始价时
/// Up 公允价 > 0.5，反之 < 0.5。Up/Down 公允价互补，各自在公允价两侧挂买一/卖一。
fn snapshot_from_price(
    price: f64,
    start_price: f64,
    half_spread: Decimal,
    sensitivity: f64,
) -> MarketSnapshot {
    let up_fair = up_fair_probability(price, start_price, sensitivity);
    MarketSnapshot {
        up: book_around(up_fair, half_spread),
        down: book_around(Decimal::ONE - up_fair, half_spread),
    }
}

/// 由价格偏离映射出 Up 侧公允概率，压缩到 (0.01, 0.99) 避免触及 0/1 边界。
fn up_fair_probability(price: f64, start_price: f64, sensitivity: f64) -> Price {
    // 用 logistic 函数把无界的相对偏离压缩到 (0, 1)，sensitivity 控制斜率陡峭程度。
    let relative = (price - start_price) / start_price;
    let logistic = 1.0 / (1.0 + (-sensitivity * relative).exp());
    let clamped = logistic.clamp(0.01, 0.99);
    Decimal::from_f64(clamped)
        .unwrap_or(Decimal::ONE / Decimal::TWO)
        .round_dp(2)
}

/// 围绕公允价构造买一/卖一盘口。
fn book_around(fair: Price, half_spread: Decimal) -> BookTop {
    BookTop {
        best_bid: Some((fair - half_spread).max(Decimal::ZERO)),
        best_ask: Some((fair + half_spread).min(Decimal::ONE)),
        last_trade: Some(fair),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn lcg_is_deterministic_for_same_seed() {
        let mut a = Lcg::new(42);
        let mut b = Lcg::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_unit(), b.next_unit());
        }
    }

    #[test]
    fn lcg_unit_stays_in_range() {
        let mut rng = Lcg::new(7);
        for _ in 0..1000 {
            let x = rng.next_unit();
            assert!((0.0..1.0).contains(&x));
        }
    }

    #[test]
    fn generate_produces_requested_number_of_snapshots() {
        let config = SyntheticMarketConfig {
            steps: 150,
            ..SyntheticMarketConfig::default()
        };
        let market = generate(&config);
        assert_eq!(market.snapshots.len(), 150);
    }

    #[test]
    fn strong_uptrend_makes_up_the_winner() {
        // 大幅正漂移、低波动 → 价格单边上行 → Up 胜出。
        let config = SyntheticMarketConfig {
            drift: 100.0,
            volatility: 1.0,
            steps: 100,
            ..SyntheticMarketConfig::default()
        };
        let market = generate(&config);
        assert_eq!(market.winner, Side::Up);
    }

    #[test]
    fn strong_downtrend_makes_down_the_winner() {
        let config = SyntheticMarketConfig {
            drift: -100.0,
            volatility: 1.0,
            steps: 100,
            ..SyntheticMarketConfig::default()
        };
        let market = generate(&config);
        assert_eq!(market.winner, Side::Down);
    }

    #[test]
    fn fair_probability_within_bounds() {
        // 任意价格映射出的盘口价都应落在 (0, 1) 内。
        let snap = snapshot_from_price(150_000.0, 100_000.0, dec!(0.01), 80.0);
        let up_ask = snap.up.best_ask.unwrap();
        assert!(up_ask > Decimal::ZERO && up_ask <= Decimal::ONE);
    }

    #[test]
    fn uptrend_raises_up_fair_above_half() {
        // 价格高于起始价 → Up 公允概率 > 0.5。
        let fair = up_fair_probability(110_000.0, 100_000.0, 80.0);
        assert!(fair > dec!(0.5));
    }
}
