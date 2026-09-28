//! 通用 HTTP 数据源 —— 兜底路径。
//!
//! 内置的百炼预设走的是未公开接口，随时可能失效。失效时用户可以用这一条
//! 自己接上任意接口：填 URL、选认证方式、用探测界面点选字段映射。
//!
//! 认证**必须支持 Cookie 与自定义 header**——控制台类内部接口只认这两种，
//! 只做 Bearer 的话根本接不上。

use chrono::Utc;
use serde_json::Value;

use crate::config::{AuthStyle, GenericCfg, WindowMapping};
use crate::usage::error::FetchError;
use crate::usage::path::{resolve_datetime, resolve_number};
use crate::usage::{UsageSnapshot, UsageWindow};

/// 认证信息落到请求上的结果。
#[derive(Debug, Default, PartialEq)]
pub struct AuthApplication {
    pub headers: Vec<(String, String)>,
    pub query: Vec<(String, String)>,
}

/// 把认证方式翻译成要加的 header 和查询参数。纯函数，便于测试。
pub fn apply_auth(auth: &AuthStyle, credential: &str) -> AuthApplication {
    let mut out = AuthApplication::default();
    let cred = credential.trim();
    if cred.is_empty() {
        return out;
    }

    match auth {
        AuthStyle::Bearer => out
            .headers
            .push(("authorization".into(), format!("Bearer {cred}"))),
        AuthStyle::ApiKeyHeader => out.headers.push(("x-api-key".into(), cred.into())),
        AuthStyle::Cookie => out.headers.push(("cookie".into(), cred.into())),
        AuthStyle::CustomHeader { name, prefix } => out.headers.push((
            name.trim().to_ascii_lowercase(),
            format!("{prefix}{cred}"),
        )),
        AuthStyle::QueryParam { name } => {
            out.query.push((name.clone(), cred.into()));
        }
        AuthStyle::None => {}
    }
    out
}

/// 用映射把响应解析成若干额度窗口。
pub fn apply_mapping(root: &Value, mappings: &[WindowMapping]) -> Vec<UsageWindow> {
    mappings
        .iter()
        .map(|m| {
            let mut w = UsageWindow::new(if m.label.trim().is_empty() {
                "额度"
            } else {
                m.label.trim()
            });

            // scale 用来适配"接口返回 0–1 小数"的情况，填 100 即可
            let scaled = |path: &Option<String>| -> Option<f64> {
                let p = path.as_ref()?;
                resolve_number(root, p).map(|n| n * m.scale)
            };

            w.remaining_pct = scaled(&m.remaining_pct).map(|p| p.clamp(0.0, 100.0));
            w.used = scaled(&m.used);
            w.total = scaled(&m.total);
            w.reset_at = m
                .reset_at
                .as_ref()
                .and_then(|p| resolve_datetime(root, p));

            w
        })
        .collect()
}

/// 发一次请求并按映射解析。
pub async fn fetch(
    http: &reqwest::Client,
    cfg: &GenericCfg,
    credential: Option<&str>,
) -> Result<UsageSnapshot, FetchError> {
    if cfg.url.trim().is_empty() {
        return Err(FetchError::NotConfigured {
            detail: "还没填接口地址".into(),
        });
    }
    if cfg.windows.is_empty() {
        return Err(FetchError::NotConfigured {
            detail: "还没配置字段映射，请用「探测」选好字段".into(),
        });
    }

    let cred = credential.unwrap_or("");
    let auth = apply_auth(&cfg.auth, cred);

    // 查询参数认证：拼到 URL 上
    let mut url = cfg.url.trim().to_string();
    if !auth.query.is_empty() {
        let qs = auth
            .query
            .iter()
            .map(|(k, v)| format!("{}={}", encode_component(k), encode_component(v)))
            .collect::<Vec<_>>()
            .join("&");
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str(&qs);
    }

    let method = cfg.method.trim().to_ascii_uppercase();
    let mut req = match method.as_str() {
        "POST" => http.post(&url),
        "PUT" => http.put(&url),
        _ => http.get(&url),
    };

    for (k, v) in &cfg.extra_headers {
        if k.trim().is_empty() {
            continue;
        }
        req = req.header(k.trim(), v);
    }
    for (k, v) in &auth.headers {
        req = req.header(k.as_str(), v);
    }

    if let Some(tpl) = cfg.body_template.as_ref().filter(|t| !t.trim().is_empty()) {
        // 只有 POST/PUT 带 body
        if method == "POST" || method == "PUT" {
            let body = tpl.replace("{{credential}}", cred);
            req = req
                .header("content-type", "application/json")
                .body(body);
        }
    }

    let resp = req.send().await.map_err(|e| FetchError::Network {
        detail: format!("{e}"),
    })?;

    let status = resp.status();
    let text = resp.text().await.map_err(|e| FetchError::Network {
        detail: format!("读取响应失败: {e}"),
    })?;

    if !status.is_success() {
        return Err(FetchError::Http {
            status: status.as_u16(),
            detail: truncate(&text, 200),
        });
    }

    let json: Value = serde_json::from_str(&text).map_err(|e| FetchError::Parse {
        detail: format!("响应不是合法 JSON: {e}；原文片段: {}", truncate(&text, 200)),
    })?;

    // 同样要展开双重编码的信封
    let json = super::oneconsole::expand_embedded_json(&json);
    let windows = apply_mapping(&json, &cfg.windows);

    if windows.is_empty() {
        return Err(FetchError::Parse {
            detail: "字段映射没有产出任何额度窗口".into(),
        });
    }

    let mut snap = UsageSnapshot::empty("自定义接口");
    snap.windows = windows;
    snap.last_success = Some(Utc::now());
    Ok(snap)
}

fn encode_component(s: &str) -> String {
    super::aliyun_sign::percent_encode(s)
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

    #[test]
    fn bearer_auth_sets_authorization_header() {
        let a = apply_auth(&AuthStyle::Bearer, "sk-abc");
        assert_eq!(
            a.headers,
            vec![("authorization".to_string(), "Bearer sk-abc".to_string())]
        );
        assert!(a.query.is_empty());
    }

    #[test]
    fn cookie_auth_sets_cookie_header() {
        // 控制台类接口的唯一接法
        let a = apply_auth(&AuthStyle::Cookie, "cna=1; csrf=2");
        assert_eq!(a.headers[0].0, "cookie");
        assert_eq!(a.headers[0].1, "cna=1; csrf=2");
    }

    #[test]
    fn custom_header_keeps_name_and_prefix() {
        let a = apply_auth(
            &AuthStyle::CustomHeader {
                name: "X-XSRF-Token".into(),
                prefix: "Token ".into(),
            },
            "abc",
        );
        // header 名要小写化
        assert_eq!(a.headers[0].0, "x-xsrf-token");
        assert_eq!(a.headers[0].1, "Token abc");
    }

    #[test]
    fn query_param_auth_goes_to_query() {
        let a = apply_auth(&AuthStyle::QueryParam { name: "key".into() }, "v1");
        assert!(a.headers.is_empty());
        assert_eq!(a.query, vec![("key".to_string(), "v1".to_string())]);
    }

    #[test]
    fn none_auth_adds_nothing() {
        let a = apply_auth(&AuthStyle::None, "ignored");
        assert_eq!(a, AuthApplication::default());
    }

    #[test]
    fn empty_credential_adds_nothing() {
        // 避免发出 "Bearer " 这种空凭据
        for style in [
            AuthStyle::Bearer,
            AuthStyle::ApiKeyHeader,
            AuthStyle::Cookie,
            AuthStyle::None,
        ] {
            let a = apply_auth(&style, "   ");
            assert!(a.headers.is_empty(), "空凭据不该产生 header: {style:?}");
            assert!(a.query.is_empty());
        }
    }

    #[test]
    fn mapping_extracts_all_fields() {
        let root = json!({
            "data": {
                "remain_percent": 62.5,
                "used": 7500,
                "total": 20000,
                "reset_time": "2026-10-01T00:00:00Z"
            }
        });

        let m = WindowMapping {
            label: "本月".into(),
            remaining_pct: Some("data.remain_percent".into()),
            used: Some("data.used".into()),
            total: Some("data.total".into()),
            reset_at: Some("data.reset_time".into()),
            scale: 1.0,
        };

        let ws = apply_mapping(&root, &[m]);
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].remaining_pct, Some(62.5));
        assert_eq!(ws[0].used, Some(7500.0));
        assert_eq!(ws[0].total, Some(20000.0));
        assert_eq!(ws[0].reset_at.unwrap().to_rfc3339(), "2026-10-01T00:00:00+00:00");
    }

    /// 接口返回 0–1 比值时，scale 填 100 就能转成百分比。
    #[test]
    fn scale_converts_ratio_to_percent() {
        let root = json!({ "pct": 0.62 });
        let m = WindowMapping {
            label: "本月".into(),
            remaining_pct: Some("pct".into()),
            used: None,
            total: None,
            reset_at: None,
            scale: 100.0,
        };
        let ws = apply_mapping(&root, &[m]);
        assert_eq!(ws[0].remaining_pct, Some(62.0));
    }

    #[test]
    fn missing_paths_yield_none_not_panic() {
        let root = json!({ "a": 1 });
        let m = WindowMapping {
            label: "本月".into(),
            remaining_pct: Some("nope".into()),
            used: Some("also.nope".into()),
            total: None,
            reset_at: Some("bad..path".into()),
            scale: 1.0,
        };
        let ws = apply_mapping(&root, &[m]);
        assert_eq!(ws[0].remaining_pct, None);
        assert_eq!(ws[0].used, None);
        assert_eq!(ws[0].reset_at, None);
    }

    #[test]
    fn supports_multiple_windows() {
        let root = json!({ "five_hour": 0.2, "weekly": 0.5 });
        let mk = |label: &str, path: &str| WindowMapping {
            label: label.into(),
            remaining_pct: Some(path.into()),
            used: None,
            total: None,
            reset_at: None,
            scale: 100.0,
        };
        let ws = apply_mapping(&root, &[mk("5小时", "five_hour"), mk("7天", "weekly")]);
        assert_eq!(ws.len(), 2);
        assert_eq!(ws[0].label, "5小时");
        assert_eq!(ws[0].remaining_pct, Some(20.0));
        assert_eq!(ws[1].remaining_pct, Some(50.0));
    }

    #[test]
    fn blank_label_gets_a_fallback() {
        let root = json!({ "pct": 1 });
        let m = WindowMapping {
            label: "   ".into(),
            remaining_pct: Some("pct".into()),
            used: None,
            total: None,
            reset_at: None,
            scale: 1.0,
        };
        let ws = apply_mapping(&root, &[m]);
        assert_eq!(ws[0].label, "额度");
    }

    #[test]
    fn percentage_is_clamped() {
        let root = json!({ "pct": 150.0 });
        let m = WindowMapping {
            label: "本月".into(),
            remaining_pct: Some("pct".into()),
            used: None,
            total: None,
            reset_at: None,
            scale: 1.0,
        };
        let ws = apply_mapping(&root, &[m]);
        assert_eq!(ws[0].remaining_pct, Some(100.0));
    }
}