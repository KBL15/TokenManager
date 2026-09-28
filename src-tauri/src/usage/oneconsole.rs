//! 阿里云「OneConsole 数据网关」客户端。
//!
//! 这不是 REST API。控制台前端的所有数据请求都打同一个入口，用 `api=` 查询参数
//! 指定内层 API 名：
//!
//! ```text
//! POST https://<gateway>/data/api.json
//!        ?action=BroadScopeAspnGateway&product=sfm_bailian&api=<内层API>&_v=undefined
//! ```
//!
//! 认证是**浏览器 Cookie + sec_token(CSRF)**，不是 AccessKey 签名。
//!
//! 契约来源是两个 MIT 协议的逆向项目（`steipete/CodexBar`、
//! `xinghe-1018/dsh-token-plan-quota`），**不是官方文档**。属未公开接口，
//! 随时可能变——所以工具保留了通用探测兜底。

use serde::Serialize;
use serde_json::{json, Value};

use super::error::{classify_gateway_code, FetchError};

/// 网关会校验 UA 像不像浏览器，用自定义 UA 会被拒（`PostonlyOrTokenError`）。
/// 两个逆向项目都踩过这个坑，所以这里固定用一个常见的 Chrome UA。
pub const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                              (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// 一次网关调用所需的全部配置。
///
/// 拆成这么多字段是为了同时支持大陆/国际、百炼/千问几种外壳——它们的
/// host、consoleSite、action 各不相同，但请求结构完全一致。
#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OneConsoleCfg {
    /// 数据网关 host。⚠️ 与页面 host 不是同一个域
    pub gateway_host: String,
    /// 页面 host，用于填 origin / referer
    pub page_host: String,
    /// 网关的 action 参数
    pub action: String,
    /// 产品标识，百炼为 `sfm_bailian`
    pub product: String,
    /// 内层 cornerstoneParam 里的 consoleSite
    pub console_site: String,
    /// 地域，如 `cn-beijing`
    pub region: String,
    /// 语言，如 `zh-CN`
    pub language: String,
    /// 内层 cornerstoneParam 里的 productCode，百炼为 `p_efm`
    pub product_code: String,

    /// 整行 Cookie 头。**整行复制，不要挑字段**
    pub cookie: String,

    /// CSRF 令牌。留空时自动从 Cookie 里的 `login_aliyunid_csrf` / `csrf` 取
    pub sec_token: Option<String>,
}

impl OneConsoleCfg {
    /// 百炼 Token Plan（中国大陆，个人版）—— 主路径默认值。
    pub fn bailian_cn() -> Self {
        Self {
            gateway_host: "bailian-cs.console.aliyun.com".into(),
            page_host: "bailian.console.aliyun.com".into(),
            action: "BroadScopeAspnGateway".into(),
            product: "sfm_bailian".into(),
            console_site: "BAILIAN_ALIYUN".into(),
            region: "cn-beijing".into(),
            language: "zh-CN".into(),
            product_code: "p_efm".into(),
            cookie: String::new(),
            sec_token: None,
        }
    }

    /// 百炼 Token Plan（国际站）
    pub fn bailian_intl() -> Self {
        Self {
            gateway_host: "bailian-singapore-cs.alibabacloud.com".into(),
            page_host: "modelstudio.console.alibabacloud.com".into(),
            action: "IntlBroadScopeAspnGateway".into(),
            product: "sfm_bailian".into(),
            // 注意这是官方历史拼写错误（ALBABACLOUD），照抄不要"修正"
            console_site: "MODELSTUDIO_ALBABACLOUD".into(),
            region: "ap-southeast-1".into(),
            language: "en-US".into(),
            product_code: "p_efm".into(),
            cookie: String::new(),
            sec_token: None,
        }
    }

    /// 千问AI平台外壳
    pub fn qianwen() -> Self {
        Self {
            gateway_host: "cs-data.qianwenai.com".into(),
            page_host: "platform.qianwenai.com".into(),
            action: "BroadScopeAspnGateway".into(),
            product: "sfm_bailian".into(),
            console_site: "QIANWENAI".into(),
            region: "cn-beijing".into(),
            language: "zh-CN".into(),
            product_code: "p_efm".into(),
            cookie: String::new(),
            sec_token: None,
        }
    }

    /// 解析出实际要用的 CSRF 令牌：显式配置优先，其次从 Cookie 里挖。
    pub fn effective_sec_token(&self) -> Option<String> {
        if let Some(t) = self.sec_token.as_ref().filter(|t| !t.trim().is_empty()) {
            return Some(t.trim().to_string());
        }
        cookie_value(&self.cookie, &["login_aliyunid_csrf", "csrf"])
            .or_else(|| cookie_value(&self.cookie, &["sec_token"]))
    }
}

/// 从 Cookie 头里取某个字段的值。Cookie 值可能带引号，会去掉。
///
/// `names` 表达的是**优先级**，不是出现顺序：外层遍历 `names`，所以
/// `["login_aliyunid_csrf", "csrf"]` 会优先取前者，哪怕后者在 Cookie 里出现得更早。
/// 这两种语义在 CSRF 上结果通常一样，但按优先级更贴合调用方的意图。
pub fn cookie_value(cookie_header: &str, names: &[&str]) -> Option<String> {
    for name in names {
        for part in cookie_header.split(';') {
            let part = part.trim();
            let Some((k, v)) = part.split_once('=') else {
                continue;
            };
            if !k.trim().eq_ignore_ascii_case(name) {
                continue;
            }
            let v = v.trim().trim_matches('"');
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// 递归展开被当成字符串塞进来的 JSON。
///
/// 网关有时回 `{"data": "{\"foo\":1}"}` 这种双重编码，不展开就会在字符串上找字段、
/// 永远找不到。CodexBar 专门写了个 `expandEmbeddedJSON` 处理这事。
pub fn expand_embedded_json(v: &Value) -> Value {
    match v {
        Value::String(s) => {
            let t = s.trim();
            // 只看像 JSON 容器的，避免把普通字符串瞎解析
            if (t.starts_with('{') && t.ends_with('}')) || (t.starts_with('[') && t.ends_with(']')) {
                match serde_json::from_str::<Value>(t) {
                    Ok(parsed) => expand_embedded_json(&parsed),
                    // 解析不了就原样保留，不要丢数据
                    Err(_) => v.clone(),
                }
            } else {
                v.clone()
            }
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, val)| (k.clone(), expand_embedded_json(val)))
                .collect(),
        ),
        Value::Array(arr) => Value::Array(arr.iter().map(expand_embedded_json).collect()),
        other => other.clone(),
    }
}

/// 在整棵树里找**第一个包含任一指定键**的对象。
///
/// 之所以不按固定路径取字段：不同版本的响应嵌套位置不一样，CodexBar 也是这么做的
/// （`findObject(containingAnyOf:)`）。按路径取的话接口一改版就全瞎。
pub fn find_object_containing<'a>(root: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    match root {
        Value::Object(map) => {
            if keys.iter().any(|k| map.contains_key(*k)) {
                return Some(root);
            }
            map.values().find_map(|v| find_object_containing(v, keys))
        }
        Value::Array(arr) => arr.iter().find_map(|v| find_object_containing(v, keys)),
        _ => None,
    }
}

/// 把值当字符串取出来。网关的 `code` 字段有时是字符串 `"200"`、有时是数字 `200`。
pub fn as_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// 构造 `params` 表单字段里的那段 JSON。
///
/// ⚠️ **绝不包含 `switchAgent`**：它绑定到具体账号的 workspace，一旦写死，
/// 换账号就会全部报 `BailianGateway.Workspace.NotAuthorised`。省略它，让网关
/// 用会话默认 workspace。
pub fn build_params_json(
    cfg: &OneConsoleCfg,
    inner_api: &str,
    trace_id: &str,
    anonymous_id: Option<&str>,
) -> Value {
    let mut cornerstone = json!({
        "feTraceId": trace_id,
        "feURL": format!("https://{}/{}?tab=plan#/efm/subscription/token-plan/personal", cfg.page_host, cfg.region),
        "protocol": "V2",
        "console": "ONE_CONSOLE",
        "productCode": cfg.product_code,
        "switchUserType": 3,
        "domain": cfg.page_host,
        "consoleSite": cfg.console_site,
        "userNickName": "",
        "userPrincipalName": "",
        "xsp_lang": cfg.language,
    });

    // X-Anonymous-Id 取自 Cookie 里的 cna，没有就不带
    if let Some(id) = anonymous_id {
        if let Some(obj) = cornerstone.as_object_mut() {
            obj.insert("X-Anonymous-Id".into(), json!(id));
        }
    }

    json!({
        "Api": inner_api,
        "V": "1.0",
        "Data": { "cornerstoneParam": cornerstone }
    })
}

/// 发一次网关调用，返回**已展开信封**的业务数据。
pub async fn call_api(
    http: &reqwest::Client,
    cfg: &OneConsoleCfg,
    inner_api: &str,
) -> Result<Value, FetchError> {
    if cfg.cookie.trim().is_empty() {
        return Err(FetchError::NotConfigured {
            detail: "还没填 Cookie".into(),
        });
    }

    let sec_token = cfg.effective_sec_token().ok_or_else(|| FetchError::NotConfigured {
        detail: "Cookie 里找不到 CSRF 令牌，请确认粘贴的是完整的 Cookie 头".into(),
    })?;

    let url = format!(
        "https://{host}/data/api.json?action={action}&product={product}&api={api}&_v=undefined",
        host = cfg.gateway_host,
        action = cfg.action,
        product = cfg.product,
        // 内层 API 名含 `/` 和 `.`，按实测契约原样拼接，不做编码
        api = inner_api,
    );

    let params = build_params_json(
        cfg,
        inner_api,
        &uuid::Uuid::new_v4().to_string(),
        cookie_value(&cfg.cookie, &["cna"]).as_deref(),
    );

    // params 要先落到具名变量，否则临时值会在 vec 构造完就被释放
    let params_str = params.to_string();
    let form = vec![
        ("product", cfg.product.as_str()),
        ("action", cfg.action.as_str()),
        ("region", cfg.region.as_str()),
        ("language", cfg.language.as_str()),
        ("sec_token", sec_token.as_str()),
        ("params", params_str.as_str()),
    ];

    let resp = http
        .post(&url)
        .header("accept", "application/json, text/plain, */*")
        .header("cookie", &cfg.cookie)
        .header("origin", format!("https://{}", cfg.page_host))
        .header(
            "referer",
            format!(
                "https://{}/{}?tab=plan#/efm/subscription/token-plan/personal",
                cfg.page_host, cfg.region
            ),
        )
        .header("user-agent", BROWSER_UA)
        .header("x-requested-with", "XMLHttpRequest")
        .header("x-xsrf-token", &sec_token)
        .header("x-csrf-token", &sec_token)
        .form(&form)
        .send()
        .await
        .map_err(|e| FetchError::Network {
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

    // 网关的业务错误码藏在 code 字段里，HTTP 状态码是 200
    if let Some(code) = json.get("code").and_then(as_string) {
        if code != "200" && code != "0" {
            let msg = json
                .get("message")
                .or_else(|| json.get("msg"))
                .and_then(as_string)
                .unwrap_or_default();
            return Err(classify_gateway_code(&code, &msg));
        }
    }

    Ok(unwrap_envelope(&json))
}

/// 剥掉网关信封，拿到业务数据。
///
/// 信封形如 `{code, data:{DataV2:{data:{...}}}}`，但不同接口层级不一，
/// 所以逐层尝试，都不匹配就退回整个 `data`。
pub fn unwrap_envelope(json: &Value) -> Value {
    let data = json.get("data").cloned().unwrap_or_else(|| json.clone());
    let expanded = expand_embedded_json(&data);

    // 优先 data.DataV2.data
    if let Some(v) = expanded
        .get("DataV2")
        .and_then(|d| d.get("data"))
    {
        return expand_embedded_json(v);
    }
    // 其次 data.data
    if let Some(v) = expanded.get("data") {
        return expand_embedded_json(v);
    }
    expanded
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

    #[test]
    fn presets_have_distinct_hosts() {
        let cn = OneConsoleCfg::bailian_cn();
        let intl = OneConsoleCfg::bailian_intl();
        let qw = OneConsoleCfg::qianwen();

        assert_eq!(cn.gateway_host, "bailian-cs.console.aliyun.com");
        assert_eq!(cn.page_host, "bailian.console.aliyun.com");
        assert_ne!(cn.gateway_host, cn.page_host, "网关 host 与页面 host 必须是两个域");

        assert_eq!(intl.action, "IntlBroadScopeAspnGateway");
        assert_eq!(qw.gateway_host, "cs-data.qianwenai.com");
    }

    #[test]
    fn parses_cookie_fields() {
        let cookie = "cna=abc123; login_aliyunid_csrf=tok%3D1; other=x";
        assert_eq!(cookie_value(cookie, &["cna"]).as_deref(), Some("abc123"));
        assert_eq!(
            cookie_value(cookie, &["login_aliyunid_csrf"]).as_deref(),
            Some("tok%3D1")
        );
        assert_eq!(cookie_value(cookie, &["missing"]), None);
    }

    #[test]
    fn cookie_value_prefers_first_matching_name() {
        // login_aliyunid_csrf 优先于 csrf
        let cookie = "csrf=fallback; login_aliyunid_csrf=primary";
        assert_eq!(
            cookie_value(cookie, &["login_aliyunid_csrf", "csrf"]).as_deref(),
            Some("primary")
        );
    }

    #[test]
    fn cookie_value_strips_quotes_and_ignores_empty() {
        assert_eq!(cookie_value("a=\"quoted\"", &["a"]).as_deref(), Some("quoted"));
        assert_eq!(cookie_value("a=; b=2", &["a"]), None);
    }

    #[test]
    fn sec_token_falls_back_to_cookie() {
        let mut cfg = OneConsoleCfg::bailian_cn();
        cfg.cookie = "login_aliyunid_csrf=fromcookie; cna=x".into();
        assert_eq!(cfg.effective_sec_token().as_deref(), Some("fromcookie"));

        // 显式配置优先
        cfg.sec_token = Some("explicit".into());
        assert_eq!(cfg.effective_sec_token().as_deref(), Some("explicit"));

        // 空白显式配置视为没配
        cfg.sec_token = Some("   ".into());
        assert_eq!(cfg.effective_sec_token().as_deref(), Some("fromcookie"));
    }

    #[test]
    fn expands_double_encoded_json() {
        let v = json!({ "data": "{\"per1MonthPercentage\":0.3}" });
        let out = expand_embedded_json(&v);
        assert_eq!(out["data"]["per1MonthPercentage"], json!(0.3));
    }

    #[test]
    fn expands_nested_and_arrays() {
        let v = json!({ "a": "{\"b\": \"{\\\"c\\\": 1}\"}", "arr": ["{\"d\":2}"] });
        let out = expand_embedded_json(&v);
        assert_eq!(out["a"]["b"]["c"], json!(1));
        assert_eq!(out["arr"][0]["d"], json!(2));
    }

    /// 普通字符串不能被瞎解析——否则会把 "200" 之类误当 JSON
    #[test]
    fn leaves_plain_strings_alone() {
        let v = json!({ "s": "hello", "n": "200", "d": "2026-10-01T00:00:00Z" });
        assert_eq!(expand_embedded_json(&v), v);
    }

    #[test]
    fn malformed_embedded_json_is_preserved() {
        // 看着像 JSON 但解析不了，应原样保留而不是丢数据
        let v = json!({ "a": "{not valid" });
        assert_eq!(expand_embedded_json(&v)["a"], json!("{not valid"));
    }

    /// 这是 CodexBar 的做法：不按固定路径取，而是在树里搜特征字段。
    #[test]
    fn finds_object_by_any_of_keys() {
        let v = json!({
            "DataV2": { "data": { "nested": { "deeper": {
                "per1MonthPercentage": 0.25, "per1MonthResetTime": 1700003600000i64
            }}}}
        });
        let found = find_object_containing(&v, &["per1MonthPercentage", "per1WeekPercentage"]).unwrap();
        assert_eq!(found["per1MonthPercentage"], json!(0.25));
    }

    #[test]
    fn find_object_works_inside_arrays() {
        let v = json!({ "items": [ {"a":1}, {"target":true} ] });
        let found = find_object_containing(&v, &["target"]).unwrap();
        assert_eq!(found["target"], json!(true));
    }

    #[test]
    fn find_object_returns_none_when_absent() {
        let v = json!({ "a": { "b": [1, 2] } });
        assert!(find_object_containing(&v, &["nope"]).is_none());
    }

    #[test]
    fn unwraps_datav2_envelope() {
        let raw = json!({
            "code": "200",
            "data": { "DataV2": { "data": { "per1MonthPercentage": 0.4 } } }
        });
        assert_eq!(unwrap_envelope(&raw)["per1MonthPercentage"], json!(0.4));
    }

    #[test]
    fn unwraps_plain_data_envelope() {
        let raw = json!({ "code": "200", "data": { "specCode": "pro" } });
        assert_eq!(unwrap_envelope(&raw)["specCode"], json!("pro"));
    }

    /// 信封本身也可能是双重编码的——调研明确提到这点
    #[test]
    fn unwraps_double_encoded_envelope() {
        let raw = json!({
            "code": "200",
            "data": "{\"DataV2\":{\"data\":\"{\\\"per1MonthPercentage\\\":0.6}\"}}"
        });
        assert_eq!(unwrap_envelope(&raw)["per1MonthPercentage"], json!(0.6));
    }

    #[test]
    fn params_json_omits_switch_agent() {
        let cfg = OneConsoleCfg::bailian_cn();
        let p = build_params_json(&cfg, "some.api", "trace-1", Some("anon"));
        let s = p.to_string();

        // 带上 switchAgent 会让别的账号报 Workspace.NotAuthorised
        assert!(!s.contains("switchAgent"), "绝不能带 switchAgent: {s}");

        assert_eq!(p["Api"], json!("some.api"));
        assert_eq!(p["V"], json!("1.0"));
        assert_eq!(p["Data"]["cornerstoneParam"]["consoleSite"], json!("BAILIAN_ALIYUN"));
        assert_eq!(p["Data"]["cornerstoneParam"]["X-Anonymous-Id"], json!("anon"));
    }

    #[test]
    fn params_json_omits_anonymous_id_when_absent() {
        let cfg = OneConsoleCfg::bailian_cn();
        let p = build_params_json(&cfg, "some.api", "trace-1", None);
        assert!(p["Data"]["cornerstoneParam"].get("X-Anonymous-Id").is_none());
    }

    #[test]
    fn as_string_handles_both_code_types() {
        assert_eq!(as_string(&json!("200")).as_deref(), Some("200"));
        assert_eq!(as_string(&json!(200)).as_deref(), Some("200"));
        assert_eq!(as_string(&json!(null)), None);
    }
}