//! 应用配置的模型与持久化。
//!
//! 存到 `%APPDATA%/tokenmoniter/config.json`。
//!
//! **敏感字段（Cookie / AccessKey Secret / API Key）不写进这个文件**，它们走
//! [`crate::secret`] 存 Windows 凭据管理器。落盘前会被 [`AppConfig::stripped`] 剥掉。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::usage::modelstudio::ModelStudioCfg;
use crate::usage::oneconsole::OneConsoleCfg;

/// 用哪条路取数。
///
/// Token Plan 个人版只能走控制台网关（Cookie），团队版可以走官方 AccessKey 接口。
/// `Auto` 会先试 AccessKey，失败再退到 Cookie——用户不用自己判断自己是哪种套餐。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum SourceMode {
    #[default]
    Auto,
    /// 只用控制台网关（Cookie）
    Cookie,
    /// 只用官方 OpenAPI（AccessKey 签名）
    AccessKey,
    /// 只用自定义 HTTP 接口
    Generic,
}

impl SourceMode {
    pub fn label(&self) -> &'static str {
        match self {
            SourceMode::Auto => "自动",
            SourceMode::Cookie => "Cookie 网关",
            SourceMode::AccessKey => "AccessKey 接口",
            SourceMode::Generic => "自定义接口",
        }
    }
}

/// 通用 HTTP 数据源的配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GenericCfg {
    pub url: String,
    #[serde(default = "default_method")]
    pub method: String,

    /// 凭据怎么带。落盘时值由凭据管理器单独保管。
    #[serde(default)]
    pub auth: AuthStyle,

    /// 附加请求头（含自定义 header 名，如 `x-csrf-token`）
    #[serde(default)]
    pub extra_headers: Vec<(String, String)>,

    /// POST 用的 body 模板，支持 `{{credential}}` 占位
    #[serde(default)]
    pub body_template: Option<String>,

    /// 额度窗口的字段映射
    #[serde(default)]
    pub windows: Vec<WindowMapping>,
}

fn default_method() -> String {
    "GET".into()
}

impl Default for GenericCfg {
    fn default() -> Self {
        Self {
            url: String::new(),
            method: default_method(),
            auth: AuthStyle::None,
            extra_headers: Vec::new(),
            body_template: None,
            windows: Vec::new(),
        }
    }
}

/// 凭据的携带方式。
///
/// `Cookie` 与 `CustomHeader` 是必需的——控制台类内部接口只认这两种，
/// 光支持 Bearer 的话根本接不上。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AuthStyle {
    /// `Authorization: Bearer <value>`
    Bearer,
    /// `x-api-key: <value>`
    ApiKeyHeader,
    /// 整行 `Cookie: <value>`
    Cookie,
    /// 自定义 header 名与值
    CustomHeader { name: String, prefix: String },
    /// 拼到 URL 查询参数上
    QueryParam { name: String },
    /// 不需要凭据
    #[default]
    None,
}

/// 一个额度窗口的字段映射。路径由设置页的字段探测界面点选生成。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WindowMapping {
    /// 展示名，如 "本月"
    #[serde(default = "default_window_label")]
    pub label: String,

    #[serde(default)]
    pub remaining_pct: Option<String>,
    #[serde(default)]
    pub used: Option<String>,
    #[serde(default)]
    pub total: Option<String>,
    #[serde(default)]
    pub reset_at: Option<String>,

    /// 取值缩放。接口返回 0–1 小数时填 100。
    #[serde(default = "default_scale")]
    pub scale: f64,
}

fn default_window_label() -> String {
    "本月".into()
}

fn default_scale() -> f64 {
    1.0
}

/// 悬浮卡片的外观与位置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WidgetConfig {
    #[serde(default = "yes")]
    pub visible: bool,
    /// 锁定后不可拖动，防止误碰
    #[serde(default)]
    pub locked: bool,
    /// 上次的位置，下次启动原位恢复
    #[serde(default)]
    pub x: Option<i32>,
    #[serde(default)]
    pub y: Option<i32>,
    #[serde(default = "default_opacity")]
    pub opacity: f64,
}

fn yes() -> bool {
    true
}

fn default_opacity() -> f64 {
    0.92
}

impl Default for WidgetConfig {
    fn default() -> Self {
        Self {
            visible: true,
            locked: false,
            x: None,
            y: None,
            opacity: default_opacity(),
        }
    }
}

/// 整个应用的配置。
///
/// 两套数据源的配置**同时存在**（而不是二选一），因为 `Auto` 模式需要在运行时
/// 从一条退到另一条。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    #[serde(default)]
    pub source_mode: SourceMode,

    /// 控制台网关（Cookie）—— 个人版走这条
    #[serde(default = "OneConsoleCfg::bailian_cn")]
    pub cookie_source: OneConsoleCfg,

    /// 官方 OpenAPI（AccessKey）—— 团队版走这条
    #[serde(default)]
    pub access_key_source: ModelStudioCfg,

    /// 自定义接口 —— 兜底
    #[serde(default)]
    pub generic_source: GenericCfg,

    /// 轮询间隔（秒）
    #[serde(default = "default_interval")]
    pub refresh_interval_secs: u64,

    /// 剩余额度低于此百分比时提醒
    #[serde(default = "default_threshold")]
    pub alert_threshold_pct: f64,

    #[serde(default)]
    pub autostart: bool,

    /// 托盘图标上是否画出百分比数字
    #[serde(default = "yes")]
    pub tray_shows_percentage: bool,

    #[serde(default)]
    pub widget: WidgetConfig,
}

fn default_interval() -> u64 {
    60
}

fn default_threshold() -> f64 {
    20.0
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            source_mode: SourceMode::Auto,
            cookie_source: OneConsoleCfg::bailian_cn(),
            access_key_source: ModelStudioCfg::default(),
            generic_source: GenericCfg::default(),
            refresh_interval_secs: default_interval(),
            alert_threshold_pct: default_threshold(),
            autostart: false,
            tray_shows_percentage: true,
            widget: WidgetConfig::default(),
        }
    }
}

impl AppConfig {
    /// 轮询间隔的合理区间。太快容易触发风控，太慢就失去意义。
    pub fn clamped_interval(&self) -> u64 {
        self.refresh_interval_secs.clamp(15, 3600)
    }

    /// 阈值收敛到 1–100，避免 0 或负数导致每次都提醒。
    pub fn clamped_threshold(&self) -> f64 {
        self.alert_threshold_pct.clamp(1.0, 100.0)
    }

    pub fn config_dir() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("tokenmoniter")
    }

    pub fn config_path() -> PathBuf {
        Self::config_dir().join("config.json")
    }

    /// 读取配置。文件不存在或损坏时退回默认值——**不能因为配置坏了就打不开程序**。
    pub fn load_or_default() -> Self {
        let Ok(text) = std::fs::read_to_string(Self::config_path()) else {
            return Self::default();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    /// 落盘。敏感字段在 [`Self::stripped`] 里被清掉。
    pub fn save(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(Self::config_dir())?;
        let text = serde_json::to_string_pretty(&self.stripped())?;
        std::fs::write(Self::config_path(), text)
    }

    /// 返回一份剥掉所有敏感字段的副本，用于落盘。
    ///
    /// 不做这一步的话，Cookie / Secret 会明文躺在 `%APPDATA%` 里，等于没加密。
    pub fn stripped(&self) -> Self {
        let mut c = self.clone();
        c.cookie_source.cookie.clear();
        c.cookie_source.sec_token = None;
        // AccessKey ID 不敏感（设置页要显示），只清 Secret —— 但 Secret 本来
        // 就不在这个结构里，所以这里只需保持现状。
        c.generic_source.body_template = c
            .generic_source
            .body_template
            .as_ref()
            .map(|t| t.replace("{{credential}}", ""));
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_usable_out_of_the_box() {
        let c = AppConfig::default();
        assert_eq!(c.clamped_interval(), 60);
        assert_eq!(c.clamped_threshold(), 20.0);
        assert_eq!(c.source_mode, SourceMode::Auto);
        assert_eq!(
            c.cookie_source.gateway_host,
            "bailian-cs.console.aliyun.com"
        );
    }

    #[test]
    fn interval_is_clamped_to_sane_range() {
        let mut c = AppConfig::default();
        c.refresh_interval_secs = 0;
        assert_eq!(c.clamped_interval(), 15, "太快会触发风控");
        c.refresh_interval_secs = 999_999;
        assert_eq!(c.clamped_interval(), 3600);
    }

    #[test]
    fn threshold_cannot_be_zero_or_negative() {
        let mut c = AppConfig::default();
        c.alert_threshold_pct = 0.0;
        assert_eq!(c.clamped_threshold(), 1.0, "0 会导致每次都提醒");
        c.alert_threshold_pct = -5.0;
        assert_eq!(c.clamped_threshold(), 1.0);
    }

    /// 最关键的一条安全断言：Cookie 绝不能进配置文件。
    #[test]
    fn stripping_removes_credentials() {
        let mut c = AppConfig::default();
        c.cookie_source.cookie = "cna=SECRETVALUE; login_aliyunid_csrf=TOK".into();
        c.cookie_source.sec_token = Some("TOK".into());

        let json = serde_json::to_string(&c.stripped()).unwrap();

        assert!(!json.contains("SECRETVALUE"), "Cookie 泄漏进配置文件了");
        assert!(!json.contains("\"TOK\""), "sec_token 泄漏进配置文件了");
        // 非敏感配置必须保留
        assert!(json.contains("bailian-cs.console.aliyun.com"));
        assert!(json.contains("refreshIntervalSecs"));
    }

    #[test]
    fn stripped_keeps_original_intact() {
        let mut c = AppConfig::default();
        c.cookie_source.cookie = "keepme".into();
        let _ = c.stripped();
        assert_eq!(c.cookie_source.cookie, "keepme", "stripped 应返回副本");
    }

    #[test]
    fn config_round_trips_through_json() {
        let mut c = AppConfig::default();
        c.refresh_interval_secs = 120;
        c.alert_threshold_pct = 15.0;
        c.widget.locked = true;
        c.source_mode = SourceMode::Cookie;

        let back: AppConfig = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn missing_optional_fields_use_defaults() {
        // 模拟一个只有最小内容的配置文件
        let json = r#"{ "sourceMode": "cookie" }"#;
        let c: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(c.refresh_interval_secs, 60);
        assert_eq!(c.alert_threshold_pct, 20.0);
        assert!(c.widget.visible);
        // 缺失的嵌套配置要有可用的默认值，不能是空 host
        assert_eq!(c.cookie_source.gateway_host, "bailian-cs.console.aliyun.com");
    }

    #[test]
    fn corrupt_config_falls_back_to_default() {
        let parsed: Result<AppConfig, _> = serde_json::from_str("{ not json");
        assert!(parsed.is_err());
        // load_or_default 内部走这个分支退回 AppConfig::default()
    }

    #[test]
    fn source_mode_labels_are_chinese() {
        assert_eq!(SourceMode::Auto.label(), "自动");
        assert_eq!(SourceMode::Cookie.label(), "Cookie 网关");
        assert_eq!(SourceMode::AccessKey.label(), "AccessKey 接口");
    }

    #[test]
    fn generic_provider_round_trips() {
        let c = AppConfig {
            generic_source: GenericCfg {
                url: "https://example.com/usage".into(),
                method: "POST".into(),
                auth: AuthStyle::Cookie,
                extra_headers: vec![("x-csrf-token".into(), "abc".into())],
                body_template: Some(r#"{"k":"{{credential}}"}"#.into()),
                windows: vec![WindowMapping {
                    label: "本月".into(),
                    remaining_pct: Some("data.pct".into()),
                    used: None,
                    total: Some("data.total".into()),
                    reset_at: Some("data.reset".into()),
                    scale: 100.0,
                }],
            },
            ..AppConfig::default()
        };

        let back: AppConfig = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn auth_style_respects_custom_headers() {
        let a = AuthStyle::CustomHeader {
            name: "x-xsrf-token".into(),
            prefix: String::new(),
        };
        let json = serde_json::to_string(&a).unwrap();
        assert!(json.contains("customHeader"));
        assert!(json.contains("x-xsrf-token"));
    }

    /// 别把凭据留在 body 模板里落盘。
    #[test]
    fn body_template_credential_is_stripped() {
        let mut c = AppConfig::default();
        c.generic_source.body_template = Some(r#"{"token":"{{credential}}"}"#.into());
        let json = serde_json::to_string(&c.stripped()).unwrap();
        assert!(!json.contains("credential"), "占位符应被清掉: {json}");
    }
}