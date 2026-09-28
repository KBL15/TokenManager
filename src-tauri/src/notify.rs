//! 额度不足提醒的状态机。
//!
//! 关键点：**只在一段"低于阈值"的区间里提醒一次**。如果每次轮询都弹，
//! 每分钟一条通知会很快让人把通知关掉，提醒也就失效了。
//!
//! 只有剩余量**回升到阈值以上**、之后再次跌破，才算新的一轮，才会再提醒。

/// 提醒状态。需要跨轮询保持，放在应用状态里。
#[derive(Debug, Default, Clone)]
pub struct AlertState {
    /// 是否已经在当前这段"低于阈值"的区间里提醒过了
    alerted: bool,
}

impl AlertState {
    /// 判断这一轮要不要弹通知，并更新状态。
    ///
    /// - 拿不到百分比：不提醒，也不改状态（避免因一次取数失败就重置提醒）
    /// - 低于阈值且本轮还没提醒过：提醒，并记下已提醒
    /// - 回到阈值以上：清掉标记，下次再跌破时才会重新提醒
    pub fn should_alert(&mut self, remaining_pct: Option<f64>, threshold: f64) -> bool {
        let Some(pct) = remaining_pct else {
            return false;
        };

        if pct < threshold {
            if self.alerted {
                return false;
            }
            self.alerted = true;
            true
        } else {
            // 回升到阈值以上，允许下次再提醒
            self.alerted = false;
            false
        }
    }

    /// 用户改了阈值后调用：重置状态，让新阈值立刻生效。
    pub fn reset(&mut self) {
        self.alerted = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alerts_once_when_crossing_below() {
        let mut s = AlertState::default();
        assert!(s.should_alert(Some(15.0), 20.0), "首次跌破应提醒");
        assert!(!s.should_alert(Some(14.0), 20.0), "同一区间内不应重复提醒");
        assert!(!s.should_alert(Some(10.0), 20.0));
        assert!(!s.should_alert(Some(1.0), 20.0));
    }

    /// 这是这个模块存在的全部理由。
    #[test]
    fn alerts_again_after_recovering_above_threshold() {
        let mut s = AlertState::default();

        assert!(s.should_alert(Some(15.0), 20.0), "跌破 → 提醒");
        assert!(!s.should_alert(Some(18.0), 20.0), "仍在阈值下 → 不提醒");
        assert!(!s.should_alert(Some(25.0), 20.0), "回升 → 不提醒，但重置标记");
        assert!(s.should_alert(Some(19.0), 20.0), "再次跌破 → 应重新提醒");
    }

    #[test]
    fn no_alert_when_above_threshold() {
        let mut s = AlertState::default();
        assert!(!s.should_alert(Some(80.0), 20.0));
        assert!(!s.should_alert(Some(20.0), 20.0), "刚好等于阈值不算跌破");
    }

    /// 取数失败不该重置提醒状态——否则网络抖动会导致重复提醒。
    #[test]
    fn missing_data_does_not_reset_state() {
        let mut s = AlertState::default();
        assert!(s.should_alert(Some(15.0), 20.0));

        assert!(!s.should_alert(None, 20.0), "拿不到数据不提醒");
        assert!(!s.should_alert(None, 20.0));

        assert!(
            !s.should_alert(Some(15.0), 20.0),
            "中间几次取数失败不应让提醒重新触发"
        );
    }

    #[test]
    fn reset_makes_next_breach_alert_again() {
        let mut s = AlertState::default();
        assert!(s.should_alert(Some(10.0), 20.0));
        assert!(!s.should_alert(Some(10.0), 20.0));

        // 用户把阈值从 20 调到 50 后，应能立刻按新阈值提醒
        s.reset();
        assert!(s.should_alert(Some(10.0), 50.0));
    }

    #[test]
    fn zero_remaining_alerts() {
        let mut s = AlertState::default();
        assert!(s.should_alert(Some(0.0), 20.0), "额度耗尽必须提醒");
    }
}