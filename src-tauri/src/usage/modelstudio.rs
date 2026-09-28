//! 百炼 Token Plan 的 **AccessKey 接口**数据源（团队版）。
//!
//! 走官方 OpenAPI：产品 `modelstudio`、版本 `2026-02-10`、ROA 风格、
//! ACS3-HMAC-SHA256 签名。端点固定 `modelstudio.cn-beijing.aliyuncs.com`。
//!
//! ⚠️ **这一组接口是团队版（席位制）的。** 返回结构全是
//! `SeatType` / `SeatCredits` / `SeatRemainingCredits` 这类"席位"概念。
//! 个人版账号调用会拿到 `COMMON-1005 系统异常`——因为后端按调用者身份
//! 推导不出 TokenPlan 组织。个人版请改用 Cookie 数据源（[`super::bailian`]）。
//!
//! 接口路径来自官方 API 元数据
//! `https://api.aliyun.com/meta/v1/products/modelstudio/versions/2026-02-10/api-docs`，
//! 不是猜的。

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use super::aliyun_v3::{self, V3Request};
use super::error::{classify_gateway_code, FetchError};
use super::{UsageSnapshot, UsageWindow};

/// Token Plan 系列接口只部署在华北2（北京）。
pub const DEFAULT_HOST: &str = "modelstudio.cn-beijing.aliyuncs.com";
pub const API_VERSION: &str = "2026-02-10";

/// 按席位类型分组的当前周期额度。**首选单次调用。**
pub const PATH_SUBSCRIPTION_STATS: &str = "/tokenplan/subscription/stats";
pub const ACTION_SUBSCRIPTION_STATS: &str = "GetSubscriptionStats";

/// 共享包（加油包）明细，字段粒度更细。
pub const PATH_SHARED_PACKAGES: &str = "/tokenplan/subscription/shared-packages";
pub const ACTION_SHARED_PACKAGES: &str = "ListSubscriptionSharedPackages";

/// 调用 AccessKey 接口所需的配置。Secret 不放这里，走凭据管理器。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelStudioCfg {
    /// AccessKey ID。不敏感，可存配置里（设置页要显示它）。
    #[serde(default)]
    pub access_key_id: String,
    #[serde(default = "default_host")]
    pub host: String,
}

fn default_host() -> String {
    DEFAULT_HOST.to_string()
}

impl Default for ModelStudioCfg {
    fn default() -> Self {
        Self {
            access_key_id: String::new(),
            host: default_host(),
        }
    }
}

// ───────────────────────── 响应结构 ─────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct StatsResponse {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    data: Option<StatsData>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct StatsData {
    /// 订阅开始时间。界面暂未展示，保留字段以对应接口 schema。
    #[serde(default)]
    #[allow(dead_code)]
    subscription_start_time: Option<i64>,
    /// 订阅结束时间。作为 `SeatRefreshTime` 缺失时的**重置时间兜底**。
    #[serde(default)]
    subscription_end_time: Option<i64>,
    #[serde(default)]
    items: Option<Vec<SeatItem>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SeatItem {
    #[serde(default)]
    seat_type: Option<String>,
    /// 本周期刷新（重置）时间
    #[serde(default)]
    seat_refresh_time: Option<i64>,
    /// 席位数量信息。当前界面没展示，但保留字段以便日后加"已分配/总席位"那一行。
    #[serde(default)]
    #[allow(dead_code)]
    total_seats: Option<i64>,
    #[serde(default)]
    #[allow(dead_code)]
    assigned_seats: Option<i64>,
    /// 本周期席位总 Credits 额度
    #[serde(default)]
    seat_credits: Option<f64>,
    /// 本周期剩余 Credits。**文档注明 CreditsService 不可用时为 null**，
    /// 所以不能直接和总额度相减。
    #[serde(default)]
    seat_remaining_credits: Option<f64>,
}

/// 把席位类型翻译成中文展示名。
fn seat_label(seat_type: &str) -> String {
    match seat_type.trim().to_ascii_lowercase().as_str() {
        "standard" => "标准席位".into(),
        "pro" => "高级席位".into(),
        "max" => "尊享席位".into(),
        "" => "额度".into(),
        other => other.to_string(),
    }
}

/// 时间戳按量级归一化：>= 1e12 视为毫秒，否则视为秒。
///
/// 官方元数据里 description 写"毫秒"、example 却是 10 位秒值，help 页又是
/// 13 位毫秒——**同一字段三处不一致**，所以只能按数值量级判断，不能硬编码。
fn normalize_epoch(v: i64) -> Option<DateTime<Utc>> {
    use chrono::TimeZone as _;
    let (secs, nanos) = if v.abs() >= 1_000_000_000_000 {
        (v.div_euclid(1000), (v.rem_euclid(1000) as u32) * 1_000_000)
    } else {
        (v, 0)
    };
    Utc.timestamp_opt(secs, nanos).single()
}

/// 把 `GetSubscriptionStats` 的响应转成额度窗口。
///
/// 每个席位类型一个窗口。
pub fn stats_to_windows(json: &Value) -> Result<Vec<UsageWindow>, FetchError> {
    let resp: StatsResponse = serde_json::from_value(json.clone()).map_err(|e| {
        FetchError::Parse {
            detail: format!("GetSubscriptionStats 响应结构不符: {e}"),
        }
    })?;

    if !resp.success {
        return Err(classify_gateway_code(
            resp.code.as_deref().unwrap_or("Unknown"),
            resp.message.as_deref().unwrap_or(""),
        ));
    }

    let data = resp.data.unwrap_or(StatsData {
        subscription_start_time: None,
        subscription_end_time: None,
        items: None,
    });

    // 席位级的刷新时间优先；没有就退回订阅结束时间，总比不显示强
    let fallback_reset = data.subscription_end_time;

    let windows: Vec<UsageWindow> = data
        .items
        .unwrap_or_default()
        .into_iter()
        .map(|it| {
            let label = seat_label(it.seat_type.as_deref().unwrap_or(""));
            let mut w = UsageWindow::new(label);

            w.total = it.seat_credits;
            w.used = match (it.seat_credits, it.seat_remaining_credits) {
                // 剩余为 null 时算不出已用——宁可留空也不要显示错误数字
                (Some(total), Some(remain)) => Some((total - remain).max(0.0)),
                _ => None,
            };
            w.reset_at = it
                .seat_refresh_time
                .or(fallback_reset)
                .and_then(normalize_epoch);
            w
        })
        .collect();

    Ok(windows)
}

/// 发一次签名请求并解析成 JSON。
async fn call(
    http: &reqwest::Client,
    cfg: &ModelStudioCfg,
    access_key_secret: &str,
    path: &str,
    action: &str,
) -> Result<Value, FetchError> {
    if cfg.access_key_id.trim().is_empty() {
        return Err(FetchError::NotConfigured {
            detail: "还没填 AccessKey ID".into(),
        });
    }
    if access_key_secret.trim().is_empty() {
        return Err(FetchError::NotConfigured {
            detail: "还没填 AccessKey Secret".into(),
        });
    }

    let req: V3Request = aliyun_v3::build_request(
        "GET",
        cfg.host.trim(),
        path,
        action,
        API_VERSION,
        &aliyun_v3::now_iso8601(),
        &aliyun_v3::new_nonce(),
        Vec::new(),
        Vec::new(),
    );

    let headers = aliyun_v3::signed_headers(&req, cfg.access_key_id.trim(), access_key_secret);
    let url = format!("https://{}{}", cfg.host.trim(), path);

    let mut rb = http.get(&url);
    for (k, v) in &headers {
        rb = rb.header(k.as_str(), v.as_str());
    }

    let resp = rb.send().await.map_err(|e| FetchError::Network {
        detail: format!("{e}"),
    })?;

    let status = resp.status();
    let text = resp.text().await.map_err(|e| FetchError::Network {
        detail: format!("读取响应失败: {e}"),
    })?;

    // 注意：这组接口的业务错误也是 HTTP 200，错误信息在 body 里，
    // 所以不能只看状态码。
    let json: Value = serde_json::from_str(&text).map_err(|_| FetchError::Http {
        status: status.as_u16(),
        detail: format!("响应不是 JSON: {}", text.chars().take(200).collect::<String>()),
    })?;

    if !status.is_success() {
        // 403 的 AccessDeniedDetail 里带 AuthAction，指出来更好排查
        let auth_action = json
            .get("AccessDeniedDetail")
            .and_then(|d| d.get("AuthAction"))
            .and_then(|a| a.as_str())
            .unwrap_or("");
        return Err(FetchError::Auth {
            detail: format!(
                "HTTP {} 被拒。{}请确认 RAM 用户已授予 AliyunTokenPlanReadOnlyAccess",
                status.as_u16(),
                if auth_action.is_empty() {
                    String::new()
                } else {
                    format!("缺少权限 {auth_action}；")
                }
            ),
        });
    }

    Ok(json)
}

/// 拉取并合成一份快照。
///
/// 先试 `GetSubscriptionStats`（一次调用拿全），失败再退到共享包接口。
pub async fn fetch(
    http: &reqwest::Client,
    cfg: &ModelStudioCfg,
    access_key_secret: &str,
) -> Result<UsageSnapshot, FetchError> {
    let stats = call(
        http,
        cfg,
        access_key_secret,
        PATH_SUBSCRIPTION_STATS,
        ACTION_SUBSCRIPTION_STATS,
    )
    .await;

    let windows = match stats {
        Ok(json) => match stats_to_windows(&json) {
            Ok(ws) if !ws.is_empty() => ws,
            Ok(_) => Vec::new(),
            Err(e) => return Err(e),
        },
        Err(e) => return Err(with_personal_plan_hint(e)),
    };

    if windows.is_empty() {
        // 席位接口返回空列表，通常意味着这是个个人版账号
        return Err(FetchError::Parse {
            detail: "接口返回成功但没有席位数据。若你是 Token Plan 个人版，\
                     这组接口（席位制）不适用，请改用 Cookie 数据源。"
                .into(),
        });
    }

    let mut snap = UsageSnapshot::empty("百炼 Token Plan（AccessKey）");
    snap.windows = windows;
    snap.unit = Some("Credits".into());
    snap.last_success = Some(Utc::now());
    Ok(snap)
}

/// 给「系统异常」这类模糊错误补上可操作的提示。
///
/// 个人版账号打团队版接口时，后端就是回一句泛泛的 `系统异常`，不补提示的话
/// 用户完全无从下手。
fn with_personal_plan_hint(e: FetchError) -> FetchError {
    match e {
        FetchError::Parse { detail } if detail.contains("COMMON-1005") => FetchError::Parse {
            detail: format!(
                "{detail}。这组接口是团队版（席位制）的，个人版账号无法使用——\
                 请在设置页改用 Cookie 数据源。"
            ),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 官方元数据里的 responseDemo 原样
    #[test]
    fn parses_official_stats_response() {
        let v = json!({
            "Success": true,
            "Code": "200",
            "Message": "successful",
            "Data": {
                "SubscriptionStartTime": 1781422733i64,
                "SubscriptionEndTime": 1781422734i64,
                "Items": [{
                    "SeatType": "standard",
                    "SeatRefreshTime": 1781422734i64,
                    "TotalSeats": 2,
                    "AssignedSeats": 1,
                    "SeatCredits": 100,
                    "SeatRemainingCredits": 40
                }]
            }
        });

        let ws = stats_to_windows(&v).unwrap();
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].label, "标准席位");
        assert_eq!(ws[0].total, Some(100.0));
        // 已用没有独立字段，必须自己相减
        assert_eq!(ws[0].used, Some(60.0));
        assert_eq!(ws[0].remaining_amount(), Some(40.0));
        assert_eq!(ws[0].effective_remaining_pct(), Some(40.0));
        assert!(ws[0].reset_at.is_some());
    }

    /// 10 位秒值应被当作秒，不是毫秒。
    #[test]
    fn normalizes_ten_digit_seconds() {
        let dt = normalize_epoch(1781422733).unwrap();
        // 若是毫秒会被解析成 1970 年，这里断言年份合理
        assert!(dt.format("%Y").to_string().starts_with("202"), "得到 {dt}");
    }

    #[test]
    fn normalizes_thirteen_digit_millis_to_same_instant() {
        let secs = normalize_epoch(1781422733).unwrap();
        let millis = normalize_epoch(1781422733000).unwrap();
        assert_eq!(secs, millis, "同一时刻的秒/毫秒写法应归一化到同一时间点");
    }

    /// 剩余额度为 null 时算不出已用，必须留空而不是显示错误数字。
    #[test]
    fn null_remaining_credits_does_not_produce_wrong_used() {
        let v = json!({
            "Success": true,
            "Data": { "Items": [{
                "SeatType": "pro",
                "SeatCredits": 100,
                "SeatRemainingCredits": null
            }]}
        });

        let ws = stats_to_windows(&v).unwrap();
        assert_eq!(ws[0].total, Some(100.0));
        assert_eq!(ws[0].used, None, "剩余未知时不能编造已用量");
        assert_eq!(ws[0].effective_remaining_pct(), None);
    }

    /// 席位没给刷新时间时，退回订阅结束时间做重置倒计时——总比不显示强。
    #[test]
    fn falls_back_to_subscription_end_time_for_reset() {
        let v = json!({
            "Success": true,
            "Data": {
                "SubscriptionEndTime": 1781422734000i64,
                "Items": [{ "SeatType": "standard", "SeatCredits": 100, "SeatRemainingCredits": 40 }]
            }
        });

        let ws = stats_to_windows(&v).unwrap();
        assert!(ws[0].reset_at.is_some(), "应退回订阅结束时间");
    }

    /// 席位自己的刷新时间优先于订阅结束时间。
    #[test]
    fn seat_refresh_time_takes_priority() {
        let v = json!({
            "Success": true,
            "Data": {
                "SubscriptionEndTime": 1781422734000i64,
                "Items": [{
                    "SeatType": "standard",
                    "SeatRefreshTime": 1700000000000i64,
                    "SeatCredits": 100,
                    "SeatRemainingCredits": 40
                }]
            }
        });

        let ws = stats_to_windows(&v).unwrap();
        let reset = ws[0].reset_at.unwrap();
        // 1700000000 秒 ≈ 2023-11，而 1781422734 秒 ≈ 2026-06
        assert!(
            reset.format("%Y").to_string() == "2023",
            "应优先用席位刷新时间，实际 {reset}"
        );
    }

    #[test]
    fn failed_response_becomes_error() {
        let v = json!({
            "Success": false,
            "Code": "COMMON-1005",
            "Message": "系统异常，请稍后重试"
        });
        let e = stats_to_windows(&v).unwrap_err();
        assert!(matches!(e, FetchError::Parse { .. }));
    }

    #[test]
    fn multiple_seat_types_become_multiple_windows() {
        let v = json!({
            "Success": true,
            "Data": { "Items": [
                {"SeatType":"standard","SeatCredits":100,"SeatRemainingCredits":40},
                {"SeatType":"max","SeatCredits":800,"SeatRemainingCredits":600}
            ]}
        });
        let ws = stats_to_windows(&v).unwrap();
        assert_eq!(ws.len(), 2);
        assert_eq!(ws[0].label, "标准席位");
        assert_eq!(ws[1].label, "尊享席位");
        assert_eq!(ws[1].effective_remaining_pct(), Some(75.0));
    }

    #[test]
    fn empty_items_yields_no_windows() {
        let v = json!({"Success": true, "Data": {"Items": []}});
        assert!(stats_to_windows(&v).unwrap().is_empty());
    }

    #[test]
    fn seat_labels_are_friendly() {
        assert_eq!(seat_label("standard"), "标准席位");
        assert_eq!(seat_label("PRO"), "高级席位");
        assert_eq!(seat_label("max"), "尊享席位");
        assert_eq!(seat_label(""), "额度");
        // 未知类型原样保留，不要吞掉
        assert_eq!(seat_label("enterprise"), "enterprise");
    }

    /// 个人版打团队版接口时，错误里必须带上可操作的建议。
    #[test]
    fn personal_plan_error_carries_actionable_hint() {
        let e = with_personal_plan_hint(FetchError::Parse {
            detail: "COMMON-1005: 系统异常，请稍后重试".into(),
        });
        let msg = e.user_message();
        assert!(msg.contains("个人版"), "应点明是个人版问题: {msg}");
        assert!(msg.contains("Cookie"), "应指向 Cookie 数据源: {msg}");
    }

    #[test]
    fn other_errors_are_not_rewritten() {
        let e = with_personal_plan_hint(FetchError::Network {
            detail: "timeout".into(),
        });
        assert!(matches!(e, FetchError::Network { .. }));
    }

    #[tokio::test]
    async fn missing_credentials_fail_fast() {
        let http = reqwest::Client::new();
        let cfg = ModelStudioCfg::default();
        let e = fetch(&http, &cfg, "").await.unwrap_err();
        assert!(matches!(e, FetchError::NotConfigured { .. }));
    }
}