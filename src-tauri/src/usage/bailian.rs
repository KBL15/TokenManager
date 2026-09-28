//! 百炼 Token Plan provider —— 主路径。
//!
//! 打三个内层 API，按官方口径合成快照：
//!
//! ```text
//! 剩余 = quota[specCode].monthly × (1 − usage.per1MonthPercentage)
//! ```
//!
//! 三个接口的分工：
//! - `usage`        → 已用比例（0–1）与重置时刻
//! - `subscription` → 档位 specCode、剩余天数
//! - `quota-config` → 各档位的总额度
//!
//! 三个里 `quota-config` 失败可以降级（少个总额度，百分比照样显示），
//! 但 `usage` 失败就没有意义了——它才是核心数据。

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::error::FetchError;
use super::oneconsole::{self, OneConsoleCfg};
use super::path::resolve_datetime;
use super::{UsageSnapshot, UsageWindow};

/// 当前账期已用比例 + 重置时刻。**唯一的比例来源。**
pub const API_USAGE: &str = "zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/usage";
/// 档位、剩余天数、到期时间
pub const API_SUBSCRIPTION: &str = "zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/subscription";
/// 各档位的总额度
pub const API_QUOTA_CONFIG: &str = "zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/quota-config";

/// 一个档位的额度配置。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QuotaEntry {
    /// 月度总额度。2026-09-23 前叫 `weekly`，之后改名 `monthly`
    pub monthly: Option<f64>,
    /// 5 小时窗口总额度（旧套餐还有）
    pub five_hour: Option<f64>,
}

/// 从 `usage` 响应里解出**已用**比例与重置时刻。
///
/// ⚠️ 返回的是**已用百分比**（0–100），不是剩余百分比。
///
/// 网关的字段名是 `per1MonthPercentage`，官方口径为
/// `剩余 = 总额度 × (1 − per1MonthPercentage)`——也就是说这个字段是**已消耗**的比例。
/// 早先这里直接把它当成剩余百分比用，导致剩余量被显示成实际值的两倍多：
/// 一个已用 69% 的账号会显示成"剩余 69%"。
///
/// 优先级：月度 > 周 > 5 小时。2026-09-23 起个人版只回月度。
fn parse_usage(v: &Value) -> (Option<f64>, Option<DateTime<Utc>>, &'static str) {
    const CANDIDATES: [(&str, &str, &str); 3] = [
        ("per1MonthPercentage", "per1MonthResetTime", "本月"),
        ("per1WeekPercentage", "per1WeekResetTime", "本周"),
        ("per5HourPercentage", "per5HourResetTime", "5小时"),
    ];

    for (pct_key, reset_key, label) in CANDIDATES {
        let Some(obj) = oneconsole::find_object_containing(v, &[pct_key]) else {
            continue;
        };
        let Some(raw) = obj.get(pct_key) else { continue };

        // 比值是 0–1，乘 100 转成百分比
        let pct = raw.as_f64().map(|p| (p * 100.0).clamp(0.0, 100.0));

        let reset = obj
            .get(reset_key)
            .and_then(|_| resolve_datetime(obj, reset_key));

        if pct.is_some() {
            return (pct, reset, label);
        }
    }

    (None, None, "本月")
}

/// 从 `subscription` 响应里解出档位代号。
///
/// **必须 trim**：实测 fixture 里是 `"  PRO  "`，带前后空格。
fn parse_spec_code(v: &Value) -> Option<String> {
    // 兼容几种别名
    let obj = oneconsole::find_object_containing(
        v,
        &["specCode", "spec_code", "planName", "plan_name"],
    )?;

    for key in ["specCode", "spec_code", "planName", "plan_name"] {
        if let Some(s) = obj.get(key).and_then(|x| x.as_str()) {
            let s = s.trim();
            if !s.is_empty() {
                return Some(s.to_ascii_lowercase());
            }
        }
    }
    None
}

/// 额度档位对象里的字段名（含历史别名）。2026-09-23 起 `weekly` 改名 `monthly`。
const MONTHLY_KEYS: [&str; 4] = ["monthly", "monthlyQuota", "weekly", "weeklyQuota"];
const FIVE_HOUR_KEYS: [&str; 3] = ["fiveHour", "five_hour", "fiveHourQuota"];

/// 一个对象是否"长得像额度档位"，如 `{"five_hour":700.0,"monthly":11500.0}`。
fn looks_like_quota_entry(m: &serde_json::Map<String, Value>) -> bool {
    MONTHLY_KEYS.iter().any(|k| m.contains_key(*k))
        || FIVE_HOUR_KEYS.iter().any(|k| m.contains_key(*k))
}

/// 在树里找"额度档位表"那一层，如 `{"lite":{...},"pro":{...}}`。
///
/// 判据是**这一层的子项里至少有一个长得像额度档位**——而不是"这层含有 monthly 字段"。
/// 这样不管响应外面包了几层信封、嵌套位置怎么变都能找到。
fn find_quota_table<'a>(
    v: &'a Value,
    depth: usize,
) -> Option<&'a serde_json::Map<String, Value>> {
    // 防病态嵌套
    if depth > 8 {
        return None;
    }
    let m = v.as_object()?;

    let looks_like_table = m
        .values()
        .any(|child| child.as_object().is_some_and(looks_like_quota_entry));
    if looks_like_table {
        return Some(m);
    }

    m.values().find_map(|child| find_quota_table(child, depth + 1))
}

/// 解析 `quota-config` 响应：`{"lite":{"five_hour":700,"monthly":11500}}`
///
/// 注意这里是**搜索树**而不是直接取根对象——响应可能带着 `{code,data,msg,...}`
/// 信封，也可能多包一层。之前按根对象取值，一旦信封没被剥掉就整个解析不出东西，
/// 表现为卡片上只有百分比、没有"剩余 X / 共 Y"。
fn parse_quota_map(v: &Value) -> std::collections::HashMap<String, QuotaEntry> {
    let mut out = std::collections::HashMap::new();

    let Some(table) = find_quota_table(v, 0) else {
        return out;
    };

    for (spec, entry) in table {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        // 跳过 addon_quota 这类非档位分组（它的值是 {extrabundle: N}，不含档位字段）
        if !looks_like_quota_entry(entry) {
            continue;
        }

        let pick = |keys: &[&str]| -> Option<f64> {
            keys.iter().find_map(|k| {
                entry.get(*k).and_then(|x| {
                    x.as_f64()
                        .or_else(|| x.as_str().and_then(|s| s.trim().parse().ok()))
                })
            })
        };

        out.insert(
            spec.to_ascii_lowercase().trim().to_string(),
            QuotaEntry {
                monthly: pick(&MONTHLY_KEYS),
                five_hour: pick(&FIVE_HOUR_KEYS),
            },
        );
    }

    out
}

/// 由**已用**比例和总额度推出 (剩余百分比, 已用量)。
///
/// 单独抽出来是为了能直接测——这个换算之前写反过，导致剩余量显示成实际值的两倍多。
pub fn derive_remaining(used_pct: f64, total: Option<f64>) -> (f64, Option<f64>) {
    let remaining_pct = (100.0 - used_pct).clamp(0.0, 100.0);
    let used = total.map(|t| t * used_pct / 100.0);
    (remaining_pct, used)
}

/// 拉取并合成一份 Token Plan 用量快照。
pub async fn fetch(
    http: &reqwest::Client,
    cfg: &OneConsoleCfg,
) -> Result<UsageSnapshot, FetchError> {
    // usage 是核心，失败即整体失败
    let usage_raw = oneconsole::call_api(http, cfg, API_USAGE).await?;

    // 另外两个是锦上添花，失败降级但不影响主数据
    let subscription_raw = oneconsole::call_api(http, cfg, API_SUBSCRIPTION)
        .await
        .unwrap_or(Value::Null);
    let quota_raw = oneconsole::call_api(http, cfg, API_QUOTA_CONFIG)
        .await
        .unwrap_or(Value::Null);

    let (used_pct, reset_at, label) = parse_usage(&usage_raw);

    if used_pct.is_none() {
        return Err(FetchError::Parse {
            detail: format!(
                "响应里找不到已用比例字段（per1Month/Week/5Hour Percentage）。\
                 接口可能已改版，建议到设置页重新探测。原文片段: {}",
                truncate(&usage_raw.to_string(), 300)
            ),
        });
    }

    let spec = parse_spec_code(&subscription_raw);
    let quota_map = parse_quota_map(&quota_raw);

    // 有档位 + 有额度表 → 能算出绝对量。任一缺失就只显示百分比，不编造数字。
    let entry = spec.as_ref().and_then(|s| quota_map.get(s));
    let total = entry.and_then(|e| e.monthly);

    // 网关给的是**已用**比例，剩余要自己翻过来
    let (remaining_pct, used) = derive_remaining(used_pct.unwrap_or(0.0), total);
    let remaining_pct = Some(remaining_pct);

    let mut window = UsageWindow::new(label);
    window.remaining_pct = remaining_pct;
    window.total = total;
    window.used = used;
    window.reset_at = reset_at;

    let mut snap = UsageSnapshot::empty(source_label(spec.as_deref()));
    snap.windows = vec![window];
    snap.unit = Some("Credits".into());
    snap.last_success = Some(Utc::now());

    Ok(snap)
}

/// 卡片副标题：带上档位更好认，拿不到就退回通用名。
fn source_label(spec: Option<&str>) -> String {
    match spec {
        Some(s) if !s.is_empty() => format!("百炼 Token Plan · {}", s.to_ascii_uppercase()),
        _ => "百炼 Token Plan".into(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── 以下 JSON 均取自逆向项目的测试 fixture 原样 ──

    #[test]
    fn parses_new_monthly_schema() {
        // 2026-09-23 起个人版只回月度字段
        let v = json!({ "per1MonthPercentage": 0.62, "per1MonthResetTime": 1790812800000i64 });
        let (pct, reset, label) = parse_usage(&v);

        assert_eq!(pct, Some(62.0), "0–1 的比值应被转成 0–100");
        assert_eq!(label, "本月");
        assert_eq!(reset.unwrap().to_rfc3339(), "2026-10-01T00:00:00+00:00");
    }

    #[test]
    fn parses_legacy_dual_window_schema() {
        // 旧套餐 fixture 原样
        let v = json!({
            "per5HourPercentage": 0.25,
            "per1WeekPercentage": 0.5,
            "per5HourResetTime": 1700003600000i64
        });
        let (pct, reset, label) = parse_usage(&v);

        // 周优先于 5 小时（月 > 周 > 5小时）
        assert_eq!(pct, Some(50.0));
        assert_eq!(label, "本周");
        assert!(reset.is_none(), "周的重置字段不在这个 fixture 里");
    }

    #[test]
    fn parses_zero_percentage() {
        // fixture: {"per1WeekPercentage":0} —— 0 是合法值，不能被当成"没取到"
        let v = json!({ "per1WeekPercentage": 0 });
        let (pct, _, label) = parse_usage(&v);
        assert_eq!(pct, Some(0.0));
        assert_eq!(label, "本周");
    }

    #[test]
    fn usage_found_even_when_deeply_nested() {
        // 不同版本嵌套位置不同，所以是"搜树"而不是按路径取
        let v = json!({
            "DataV2": { "data": { "result": { "quota": {
                "per1MonthPercentage": 0.15, "per1MonthResetTime": 1790812800000i64
            }}}}
        });
        let (pct, _, _) = parse_usage(&v);
        assert_eq!(pct, Some(15.0));
    }

    #[test]
    fn usage_returns_none_when_field_absent() {
        let (pct, reset, _) = parse_usage(&json!({ "unrelated": 1 }));
        assert!(pct.is_none());
        assert!(reset.is_none());
    }

    #[test]
    fn clamps_out_of_range_ratios() {
        // 防御性：万一上游给了超过 1 的比值
        let (pct, _, _) = parse_usage(&json!({ "per1MonthPercentage": 1.5 }));
        assert_eq!(pct, Some(100.0));
    }

    #[test]
    fn spec_code_is_trimmed_and_lowercased() {
        // fixture 原样：值带前后空格
        let v = json!({ "specCode": "  PRO  " });
        assert_eq!(parse_spec_code(&v).as_deref(), Some("pro"));
    }

    #[test]
    fn spec_code_accepts_aliases() {
        assert_eq!(parse_spec_code(&json!({"spec_code":"lite"})).as_deref(), Some("lite"));
        assert_eq!(parse_spec_code(&json!({"planName":"Standard"})).as_deref(), Some("standard"));
        assert_eq!(parse_spec_code(&json!({"nope":1})), None);
    }

    #[test]
    fn parses_quota_config_with_monthly() {
        // 新版：weekly 改名 monthly
        let v = json!({ "pro": {"monthly": 20000, "fiveHour": 80} });
        let m = parse_quota_map(&v);
        assert_eq!(m.get("pro").unwrap().monthly, Some(20000.0));
        assert_eq!(m.get("pro").unwrap().five_hour, Some(80.0));
    }

    #[test]
    fn parses_quota_config_legacy_weekly_and_snake_case() {
        // fixture 原样
        let v = json!({ "pro": {"fiveHour": 80, "weekly": 160}, "max": {"fiveHour": 800} });
        let m = parse_quota_map(&v);

        assert_eq!(m.get("pro").unwrap().monthly, Some(160.0), "weekly 应被当作 monthly");
        assert_eq!(m.get("pro").unwrap().five_hour, Some(80.0));
        assert_eq!(m.get("max").unwrap().monthly, None);
        assert_eq!(m.get("max").unwrap().five_hour, Some(800.0));

        // 下划线写法
        let m2 = parse_quota_map(&json!({ "lite": {"five_hour": 100, "weekly": 200} }));
        assert_eq!(m2.get("lite").unwrap().five_hour, Some(100.0));
        assert_eq!(m2.get("lite").unwrap().monthly, Some(200.0));
    }

    #[test]
    fn quota_config_tolerates_string_numbers() {
        let v = json!({ "pro": {"monthly": "20000.00"} });
        let m = parse_quota_map(&v);
        assert_eq!(m.get("pro").unwrap().monthly, Some(20000.0));
    }

    /// **回归测试**：2026-09-28 从真实账号抓到的 `quota-config` 响应原样，**带信封**。
    ///
    /// 之前这里是按根对象取值的，信封一旦没被剥掉就整个解析不出东西——
    /// 表现是卡片上只有百分比，没有"剩余 X / 共 Y"。改成搜索树之后不再依赖信封状态。
    #[test]
    fn parses_real_quota_response_with_envelope() {
        let v = json!({
            "code": "SUCCESS",
            "data": {
                "addon_quota": { "extrabundle": 20000.0 },
                "essential": { "five_hour": 1800.0, "monthly": 25500.0 },
                "lite": { "five_hour": 700.0, "monthly": 11500.0 },
                "pro": { "five_hour": 12000.0, "monthly": 180000.0 },
                "standard": { "five_hour": 3000.0, "monthly": 45000.0 }
            },
            "msg": "Success.",
            "requestId": "54991a93-0974-9602-b4fd-f96a1732989b",
            "success": true
        });

        let m = parse_quota_map(&v);

        assert_eq!(m.get("lite").unwrap().monthly, Some(11500.0), "lite 月度额度");
        assert_eq!(m.get("lite").unwrap().five_hour, Some(700.0));
        assert_eq!(m.get("pro").unwrap().monthly, Some(180000.0));
        assert_eq!(m.get("standard").unwrap().monthly, Some(45000.0));
        // addon_quota 的值是 {extrabundle: N}，不是档位，不该混进来
        assert!(!m.contains_key("addon_quota"));
        assert_eq!(m.len(), 4);
    }

    /// 信封被剥掉时同样要能解析（两种形态都得支持）。
    #[test]
    fn parses_quota_config_with_or_without_envelope() {
        let inner = json!({ "lite": { "five_hour": 700.0, "monthly": 11500.0 } });
        let wrapped = json!({ "code": "SUCCESS", "data": inner.clone(), "success": true });

        assert_eq!(parse_quota_map(&inner).get("lite").unwrap().monthly, Some(11500.0));
        assert_eq!(parse_quota_map(&wrapped).get("lite").unwrap().monthly, Some(11500.0));
    }

    /// 真实 `usage` 响应原样（带信封）——确认搜索树的方式不受信封影响。
    #[test]
    fn parses_real_usage_response_with_envelope() {
        let v = json!({
            "code": "SUCCESS",
            "data": {
                "per1MonthPercentage": 0.6450229565217391,
                "per1MonthResetTime": 1793030400000i64
            },
            "msg": "Success.",
            "success": true
        });

        let (pct, reset, label) = parse_usage(&v);
        assert_eq!(label, "本月");
        assert!((pct.unwrap() - 64.50229565217391).abs() < 1e-9);
        assert!(reset.is_some());
    }

    /// 真实 `subscription` 响应原样（带信封）。
    #[test]
    fn parses_real_subscription_response_with_envelope() {
        let v = json!({
            "code": "SUCCESS",
            "data": {
                "autoRenewFlag": false,
                "endTime": 1793030400000i64,
                "instanceCode": "sfm_tokenplansolo_public_cn-sq94z5ect0j",
                "remainingDays": 28,
                "specCode": "lite",
                "startTime": 1790410358000i64,
                "status": "VALID"
            },
            "success": true
        });

        assert_eq!(parse_spec_code(&v).as_deref(), Some("lite"));
    }

    /// 端到端：用真实响应拼出快照，`total` / `used` 必须都在。
    #[test]
    fn real_responses_produce_total_and_used() {
        let usage = json!({
            "code": "SUCCESS",
            "data": { "per1MonthPercentage": 0.6450229565217391, "per1MonthResetTime": 1793030400000i64 }
        });
        let subscription = json!({
            "code": "SUCCESS",
            "data": { "specCode": "lite", "remainingDays": 28 }
        });
        let quota = json!({
            "code": "SUCCESS",
            "data": {
                "addon_quota": { "extrabundle": 20000.0 },
                "lite": { "five_hour": 700.0, "monthly": 11500.0 }
            }
        });

        let (used_pct, _, _) = parse_usage(&usage);
        let spec = parse_spec_code(&subscription);
        let map = parse_quota_map(&quota);

        let total = spec.as_ref().and_then(|s| map.get(s)).and_then(|e| e.monthly);
        assert_eq!(total, Some(11500.0), "必须能取到总额度，否则卡片上不显示剩余量");

        let (remaining_pct, used) = derive_remaining(used_pct.unwrap(), total);
        let used = used.unwrap();

        // 11500 × 0.64502 ≈ 7417.8 已用
        assert!((used - 7417.76).abs() < 1.0, "已用量应在 7417 附近，实际 {used}");
        // 11500 − 7417.8 ≈ 4082.2 剩余
        let remaining = total.unwrap() - used;
        assert!((remaining - 4082.24).abs() < 1.0, "剩余应在 4082 附近，实际 {remaining}");
        // 已用 64.5% → 剩余 35.5%
        assert!((remaining_pct - 35.497).abs() < 0.01, "剩余百分比应约 35.5，实际 {remaining_pct}");
    }

    /// **回归测试**：网关给的是**已用**比例，剩余必须翻过来。
    ///
    /// 之前这里直接拿 `per1MonthPercentage` 当剩余百分比用，于是已用 69% 的账号
    /// 会显示"剩余 69%"——把剩余额度夸大成实际值的两倍多。
    #[test]
    fn derive_remaining_inverts_used_percentage() {
        let (remaining_pct, used) = derive_remaining(69.0, Some(11500.0));

        assert_eq!(remaining_pct, 31.0, "已用 69% → 剩余 31%");
        assert_eq!(used, Some(7935.0), "已用量 = 总额度 × 已用比例");
        // 剩余量必须是总额度减已用，而不是总额度乘已用比例
        assert_eq!(11500.0 - used.unwrap(), 3565.0);
    }

    #[test]
    fn derive_remaining_handles_edges() {
        // 一点没用
        assert_eq!(derive_remaining(0.0, Some(1000.0)), (100.0, Some(0.0)));
        // 用光了
        assert_eq!(derive_remaining(100.0, Some(1000.0)), (0.0, Some(1000.0)));
        // 超额（上游给了 >100）要夹住，不能出现负的剩余
        assert_eq!(derive_remaining(120.0, Some(1000.0)), (0.0, Some(1200.0)));
        // 没有总额度时仍能给出剩余百分比
        assert_eq!(derive_remaining(30.0, None), (70.0, None));
    }

    #[test]
    fn quota_map_survives_garbage() {
        // 非对象条目、空对象都要能扛住，不能 panic
        let v = json!({ "pro": 123, "bad": null, "ok": {"monthly": 5} });
        let m = parse_quota_map(&v);
        assert_eq!(m.get("ok").unwrap().monthly, Some(5.0));
        assert!(!m.contains_key("bad"));
    }

    /// 官方口径：剩余 = 总额度 × (1 − 已用比例)
    #[test]
    fn computes_remaining_matching_console_formula() {
        let total = 20000.0;
        let pct_used = 0.62;
        let remaining_pct = (1.0 - pct_used) * 100.0; // 38.0
        let used = total * pct_used;

        let mut w = UsageWindow::new("本月");
        w.total = Some(total);
        w.used = Some(used);
        w.remaining_pct = Some(remaining_pct);

        assert_eq!(w.remaining_pct, Some(38.0));
        assert_eq!(w.remaining_amount(), Some(total - used));
        assert_eq!(w.remaining_amount(), Some(7600.0));
    }

    #[test]
    fn source_label_includes_spec_when_known() {
        assert_eq!(
            source_label(Some("pro")),
            "百炼 Token Plan · PRO"
        );
        assert_eq!(source_label(None), "百炼 Token Plan");
        assert_eq!(source_label(Some("")), "百炼 Token Plan");
    }
}