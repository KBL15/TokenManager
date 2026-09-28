//! 用量数据的统一模型。
//!
//! 不管底层走的是签名 OpenAPI 还是通用 HTTP + 字段映射，最终都归一化成
//! [`UsageSnapshot`]，前端只认这一个结构。这样换数据源不用动 UI。

pub mod aliyun_sign;
pub mod aliyun_v3;
pub mod bailian;
pub mod error;
pub mod generic;
pub mod modelstudio;
pub mod oneconsole;
pub mod path;
pub mod provider;

// 说明：原计划里的「原始请求探测 + 点选 JSON 字段树」没有实现。
// 实际做的是——个人版走内置的百炼契约（不需要字段映射），
// 自定义接口则手工填写字段路径。可参考 README 的「接口改版了怎么办」。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// 一个额度窗口。
///
/// Token Plan 现在是**按月单一额度**，所以通常只有一个元素；
/// 但保留 Vec 结构的成本极低，万一将来又改回多窗口（历史上曾有过
/// 5 小时 / 7 天双窗口），或者用户想同时监控多个来源，架构不用动。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UsageWindow {
    /// 展示名，如 "本月" / "5小时"
    pub label: String,

    /// 剩余百分比 0–100。接口没直接给时，由 used/total 推算。
    pub remaining_pct: Option<f64>,

    pub used: Option<f64>,
    pub total: Option<f64>,

    /// 额度重置时刻（UTC）
    pub reset_at: Option<DateTime<Utc>>,
}

impl UsageWindow {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            remaining_pct: None,
            used: None,
            total: None,
            reset_at: None,
        }
    }

    /// 取剩余百分比：优先用接口直接给的，没有就用 used/total 算。
    pub fn effective_remaining_pct(&self) -> Option<f64> {
        self.remaining_pct.or_else(|| {
            let (used, total) = (self.used?, self.total?);
            path::percent_from_used_total(used, total)
        })
    }

    /// 这个窗口的剩余绝对量。used/total 都有才能算。
    pub fn remaining_amount(&self) -> Option<f64> {
        Some(self.total? - self.used?)
    }
}

/// 一次刷新的完整结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UsageSnapshot {
    pub windows: Vec<UsageWindow>,

    /// 计量单位，如 "Credits"。
    ///
    /// Token Plan 没有固定的 Token↔Credits 换算比例，所以一律按 Credits 展示，
    /// 不做 token 换算。
    pub unit: Option<String>,

    /// 这次刷新的时刻
    pub fetched_at: DateTime<Utc>,

    /// 数据源展示名，显示在卡片副标题
    pub source_label: String,

    /// 本次刷新失败时的错误描述。**失败不影响 windows 保留上一次成功的值。**
    pub error: Option<String>,

    /// 最近一次成功刷新的时刻。用于区分"数据是新的"还是"是上次的存量"。
    pub last_success: Option<DateTime<Utc>>,
}

impl UsageSnapshot {
    /// 尚未取到任何数据时的空状态。
    pub fn empty(source_label: impl Into<String>) -> Self {
        Self {
            windows: Vec::new(),
            unit: None,
            fetched_at: Utc::now(),
            source_label: source_label.into(),
            error: None,
            last_success: None,
        }
    }

    /// 界面是否该显示"数据可能已过期"。
    pub fn is_stale(&self) -> bool {
        self.error.is_some() && self.last_success.is_some()
    }

    /// 是否已经有可用数据（哪怕正在报错）。
    pub fn has_data(&self) -> bool {
        !self.windows.is_empty() && self.last_success.is_some()
    }

    /// 剩余最紧张的窗口——托盘图标上画的就是它。
    ///
    /// 取剩余百分比最低的那个；算不出百分比的窗口跳过；
    /// 一个都算不出来时返回 None，托盘改画问号。
    pub fn most_constrained(&self) -> Option<&UsageWindow> {
        self.windows
            .iter()
            .filter_map(|w| w.effective_remaining_pct().map(|p| (p, w)))
            .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(_, w)| w)
    }

    /// 刷新失败时调用：保留旧数据，只挂上错误信息。
    pub fn with_error(&self, msg: impl Into<String>) -> Self {
        Self {
            error: Some(msg.into()),
            fetched_at: Utc::now(),
            ..self.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(label: &str, remaining: Option<f64>, used: Option<f64>, total: Option<f64>) -> UsageWindow {
        UsageWindow {
            label: label.into(),
            remaining_pct: remaining,
            used,
            total,
            reset_at: None,
        }
    }

    #[test]
    fn computes_pct_from_used_total_when_api_omits_it() {
        let w = win("本月", None, Some(250.0), Some(1000.0));
        assert_eq!(w.effective_remaining_pct(), Some(75.0));
    }

    #[test]
    fn prefers_api_supplied_pct() {
        // 接口直接给了百分比就信接口的，哪怕和 used/total 对不上
        let w = win("本月", Some(50.0), Some(250.0), Some(1000.0));
        assert_eq!(w.effective_remaining_pct(), Some(50.0));
    }

    #[test]
    fn pct_is_none_when_insufficient_data() {
        assert_eq!(win("本月", None, None, None).effective_remaining_pct(), None);
        // 只有 used 没 total，算不出来
        assert_eq!(win("本月", None, Some(10.0), None).effective_remaining_pct(), None);
    }

    #[test]
    fn remaining_amount_needs_both_ends() {
        assert_eq!(win("本月", None, Some(250.0), Some(1000.0)).remaining_amount(), Some(750.0));
        assert_eq!(win("本月", None, Some(250.0), None).remaining_amount(), None);
    }

    #[test]
    fn most_constrained_picks_lowest_remaining() {
        let snap = UsageSnapshot {
            windows: vec![
                win("本月", Some(80.0), None, None),
                win("5小时", Some(12.0), None, None),
                win("7天", Some(45.0), None, None),
            ],
            ..UsageSnapshot::empty("测试")
        };
        assert_eq!(snap.most_constrained().unwrap().label, "5小时");
    }

    #[test]
    fn most_constrained_ignores_windows_without_pct() {
        let snap = UsageSnapshot {
            windows: vec![
                win("算不出", None, None, None),
                win("本月", Some(30.0), None, None),
            ],
            ..UsageSnapshot::empty("测试")
        };
        assert_eq!(snap.most_constrained().unwrap().label, "本月");

        // 全都算不出时应返回 None，而不是 panic
        let none = UsageSnapshot {
            windows: vec![win("算不出", None, None, None)],
            ..UsageSnapshot::empty("测试")
        };
        assert!(none.most_constrained().is_none());
    }

    /// 刷新失败必须保留旧数据——用量监控最忌讳网络抖一下就变空白。
    #[test]
    fn error_preserves_previous_data() {
        let snap = UsageSnapshot {
            windows: vec![win("本月", Some(62.0), Some(7600.0), Some(20000.0))],
            unit: Some("Credits".into()),
            last_success: Some(Utc::now()),
            ..UsageSnapshot::empty("百炼 Token Plan")
        };

        let failed = snap.with_error("连接超时");

        assert_eq!(failed.windows.len(), 1, "旧数据丢了");
        assert_eq!(failed.windows[0].remaining_pct, Some(62.0));
        assert_eq!(failed.error.as_deref(), Some("连接超时"));
        assert!(failed.is_stale(), "有旧数据 + 有错误，应标记为过期");
        assert!(failed.has_data(), "虽然报错但仍有数据可展示");
    }

    #[test]
    fn stale_only_when_there_is_old_data() {
        // 从没成功过就报错，不算"过期"，是"还没数据"
        let fresh_failure = UsageSnapshot::empty("测试").with_error("首次就失败");
        assert!(!fresh_failure.is_stale());
        assert!(!fresh_failure.has_data());
    }

    #[test]
    fn snapshot_serializes_to_camel_case_for_frontend() {
        let snap = UsageSnapshot {
            windows: vec![win("本月", Some(62.0), None, None)],
            ..UsageSnapshot::empty("测试")
        };
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("remainingPct"), "前端约定 camelCase: {json}");
        assert!(json.contains("sourceLabel"));
        assert!(json.contains("lastSuccess"));
    }
}