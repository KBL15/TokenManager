//! 数据源分派与自动回退。
//!
//! Token Plan 个人版只能走控制台网关（Cookie），团队版才可以走官方 AccessKey 接口。
//! 用户未必清楚自己是哪种，所以 [`SourceMode::Auto`] 会先试 AccessKey，失败再退到
//! Cookie——这在实践中是有效的，因为个人版打 AccessKey 接口会明确失败（席位制接口
//! 返回"系统异常"），而不是给出错误数据。

use crate::config::{AppConfig, SourceMode};
use crate::usage::error::FetchError;
use crate::usage::{bailian, generic, modelstudio, UsageSnapshot};

/// 运行时从凭据管理器取出来的敏感值。
///
/// **不落盘**，每次刷新时由上层注入。
#[derive(Debug, Clone, Default)]
pub struct Credentials {
    /// 控制台网关的整行 Cookie
    pub cookie: Option<String>,
    /// AccessKey Secret
    pub access_key_secret: Option<String>,
    /// 自定义接口的凭据（API Key / Token 等）
    pub generic: Option<String>,
}

impl Credentials {
    pub fn get(&self, key: &str) -> Option<&str> {
        match key {
            crate::secret::KEY_COOKIE => self.cookie.as_deref(),
            crate::secret::KEY_AK_SECRET => self.access_key_secret.as_deref(),
            crate::secret::KEY_GENERIC => self.generic.as_deref(),
            _ => None,
        }
    }
}

/// 构造统一配置的 HTTP 客户端。
///
/// 用 `native-tls`（Windows 走系统 SChannel）而不是 rustls，省掉打包整套 TLS 栈。
pub fn build_http_client() -> Result<reqwest::Client, FetchError> {
    reqwest::Client::builder()
        .user_agent(super::oneconsole::BROWSER_UA)
        .timeout(std::time::Duration::from_secs(30))
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| FetchError::Network {
            detail: format!("HTTP 客户端初始化失败: {e}"),
        })
}

/// 取一次数据，返回快照与实际生效的数据源。
pub async fn fetch(
    http: &reqwest::Client,
    cfg: &AppConfig,
    creds: &Credentials,
) -> Result<(UsageSnapshot, SourceMode), FetchError> {
    match cfg.source_mode {
        SourceMode::Cookie => Ok((fetch_cookie(http, cfg, creds).await?, SourceMode::Cookie)),
        SourceMode::AccessKey => {
            Ok((fetch_access_key(http, cfg, creds).await?, SourceMode::AccessKey))
        }
        SourceMode::Generic => Ok((fetch_generic(http, cfg, creds).await?, SourceMode::Generic)),

        SourceMode::Auto => fetch_auto(http, cfg, creds).await,
    }
}

/// 自动模式：先 AccessKey，再 Cookie，最后自定义接口。
async fn fetch_auto(
    http: &reqwest::Client,
    cfg: &AppConfig,
    creds: &Credentials,
) -> Result<(UsageSnapshot, SourceMode), FetchError> {
    let mut attempts: Vec<(SourceMode, FetchError)> = Vec::new();

    // 1) AccessKey —— 团队版适用
    let ak_configured = !cfg.access_key_source.access_key_id.trim().is_empty()
        && creds.access_key_secret.is_some();
    if ak_configured {
        match fetch_access_key(http, cfg, creds).await {
            Ok(s) => return Ok((s, SourceMode::AccessKey)),
            Err(e) => attempts.push((SourceMode::AccessKey, e)),
        }
    }

    // 2) Cookie 网关 —— 个人版适用
    let cookie_configured = creds.cookie.as_ref().is_some_and(|c| !c.trim().is_empty());
    if cookie_configured {
        match fetch_cookie(http, cfg, creds).await {
            Ok(s) => return Ok((s, SourceMode::Cookie)),
            Err(e) => attempts.push((SourceMode::Cookie, e)),
        }
    }

    // 3) 自定义接口 —— 兜底
    if !cfg.generic_source.url.trim().is_empty() {
        match fetch_generic(http, cfg, creds).await {
            Ok(s) => return Ok((s, SourceMode::Generic)),
            Err(e) => attempts.push((SourceMode::Generic, e)),
        }
    }

    Err(combine_attempts(attempts))
}

/// 把所有失败尝试合成一条能指导操作的错误。
///
/// 只说"全部失败"没用；要告诉用户每条路各自为什么失败、该去做什么。
fn combine_attempts(attempts: Vec<(SourceMode, FetchError)>) -> FetchError {
    if attempts.is_empty() {
        return FetchError::NotConfigured {
            detail: "还没有配置任何数据源。请在设置页填入 Cookie（个人版）\
                     或 AccessKey（团队版）。"
                .into(),
        };
    }

    let detail = attempts
        .iter()
        .map(|(mode, e)| format!("{}：{}", mode.label(), e.user_message()))
        .collect::<Vec<_>>()
        .join("；");

    // 如果全是认证/配置类问题，就归类为 Auth，让界面弹出"去设置"的引导
    if attempts.iter().all(|(_, e)| e.is_user_actionable()) {
        FetchError::Auth { detail }
    } else {
        FetchError::Parse { detail }
    }
}

async fn fetch_cookie(
    http: &reqwest::Client,
    cfg: &AppConfig,
    creds: &Credentials,
) -> Result<UsageSnapshot, FetchError> {
    let mut c = cfg.cookie_source.clone();
    if c.cookie.trim().is_empty() {
        c.cookie = creds.cookie.clone().unwrap_or_default();
    }
    if c.cookie.trim().is_empty() {
        return Err(FetchError::NotConfigured {
            detail: "还没填 Cookie，请到设置页粘贴".into(),
        });
    }
    bailian::fetch(http, &c).await
}

async fn fetch_access_key(
    http: &reqwest::Client,
    cfg: &AppConfig,
    creds: &Credentials,
) -> Result<UsageSnapshot, FetchError> {
    if cfg.access_key_source.access_key_id.trim().is_empty() {
        return Err(FetchError::NotConfigured {
            detail: "还没填 AccessKey ID".into(),
        });
    }
    let secret = creds.access_key_secret.as_deref().ok_or_else(|| {
        FetchError::NotConfigured {
            detail: "还没填 AccessKey Secret".into(),
        }
    })?;
    modelstudio::fetch(http, &cfg.access_key_source, secret).await
}

async fn fetch_generic(
    http: &reqwest::Client,
    cfg: &AppConfig,
    creds: &Credentials,
) -> Result<UsageSnapshot, FetchError> {
    generic::fetch(http, &cfg.generic_source, creds.generic.as_deref()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::oneconsole::OneConsoleCfg;

    #[test]
    fn builds_http_client_with_browser_ua() {
        assert!(build_http_client().is_ok());
    }

    #[test]
    fn credentials_lookup_by_key() {
        let c = Credentials {
            cookie: Some("ck".into()),
            access_key_secret: Some("sk".into()),
            generic: Some("gk".into()),
        };
        assert_eq!(c.get(crate::secret::KEY_COOKIE), Some("ck"));
        assert_eq!(c.get(crate::secret::KEY_AK_SECRET), Some("sk"));
        assert_eq!(c.get(crate::secret::KEY_GENERIC), Some("gk"));
        assert_eq!(c.get("nope"), None);
    }

    /// 什么都没配时，错误必须告诉用户下一步做什么。
    #[tokio::test]
    async fn nothing_configured_gives_actionable_error() {
        let http = build_http_client().unwrap();
        let mut cfg = AppConfig::default();
        cfg.source_mode = SourceMode::Auto;
        cfg.cookie_source = OneConsoleCfg::bailian_cn();

        let e = fetch(&http, &cfg, &Credentials::default()).await.unwrap_err();
        assert!(matches!(e, FetchError::NotConfigured { .. }));
        assert!(e.user_message().contains("Cookie"), "{}", e.user_message());
    }

    #[tokio::test]
    async fn explicit_cookie_mode_without_cookie_fails_fast() {
        let http = build_http_client().unwrap();
        let cfg = AppConfig {
            source_mode: SourceMode::Cookie,
            ..AppConfig::default()
        };
        let e = fetch(&http, &cfg, &Credentials::default()).await.unwrap_err();
        assert!(matches!(e, FetchError::NotConfigured { .. }));
    }

    #[tokio::test]
    async fn explicit_access_key_mode_without_id_fails_fast() {
        let http = build_http_client().unwrap();
        let cfg = AppConfig {
            source_mode: SourceMode::AccessKey,
            ..AppConfig::default()
        };
        let e = fetch(&http, &cfg, &Credentials::default()).await.unwrap_err();
        assert!(matches!(e, FetchError::NotConfigured { .. }));
    }

    /// 有 ID 但缺 Secret 也要明确指出缺的是哪一个。
    #[tokio::test]
    async fn access_key_mode_reports_missing_secret_specifically() {
        let http = build_http_client().unwrap();
        let mut cfg = AppConfig {
            source_mode: SourceMode::AccessKey,
            ..AppConfig::default()
        };
        cfg.access_key_source.access_key_id = "LTAI_test".into();

        let e = fetch(&http, &cfg, &Credentials::default()).await.unwrap_err();
        assert!(
            e.user_message().contains("Secret"),
            "应指明缺的是 Secret: {}",
            e.user_message()
        );
    }

    #[test]
    fn combine_reports_every_attempt() {
        let attempts = vec![
            (
                SourceMode::AccessKey,
                FetchError::Parse {
                    detail: "系统异常".into(),
                },
            ),
            (
                SourceMode::Cookie,
                FetchError::Auth {
                    detail: "登录态失效".into(),
                },
            ),
        ];
        let e = combine_attempts(attempts);
        let msg = e.user_message();
        assert!(msg.contains("AccessKey 接口"), "{msg}");
        assert!(msg.contains("Cookie 网关"), "{msg}");
    }

    /// 全是可操作的错 → 归类为 Auth，界面好据此弹「去设置」引导。
    #[test]
    fn combine_classifies_all_actionable_as_auth() {
        let attempts = vec![
            (SourceMode::AccessKey, FetchError::NotConfigured { detail: "x".into() }),
            (SourceMode::Cookie, FetchError::Auth { detail: "y".into() }),
        ];
        assert!(matches!(combine_attempts(attempts), FetchError::Auth { .. }));
    }

    /// 有网络类错误时不该误判成「去改配置」。
    #[test]
    fn combine_keeps_network_errors_out_of_auth_bucket() {
        let attempts = vec![
            (SourceMode::Cookie, FetchError::Network { detail: "timeout".into() }),
            (SourceMode::AccessKey, FetchError::Auth { detail: "y".into() }),
        ];
        assert!(matches!(combine_attempts(attempts), FetchError::Parse { .. }));
    }

    #[test]
    fn combine_with_no_attempts_is_not_configured() {
        assert!(matches!(
            combine_attempts(Vec::new()),
            FetchError::NotConfigured { .. }
        ));
    }

    /// 个人版走 Auto 时：AccessKey 失败应自动退到 Cookie，而不是直接报错放弃。
    #[tokio::test]
    async fn auto_falls_back_to_cookie_when_access_key_fails() {
        let http = build_http_client().unwrap();
        let mut cfg = AppConfig {
            source_mode: SourceMode::Auto,
            ..AppConfig::default()
        };
        // 配了 AK（会因网络/业务失败），但没配 Cookie
        cfg.access_key_source.access_key_id = "LTAI_test".into();

        let creds = Credentials {
            access_key_secret: Some("secret".into()),
            ..Credentials::default()
        };

        let e = fetch(&http, &cfg, &creds).await.unwrap_err();
        // 没有 Cookie 可退，所以最终报错里应同时提到两条路
        let msg = e.user_message();
        assert!(msg.contains("AccessKey"), "{msg}");
        assert!(msg.contains("Cookie"), "{msg}");
    }
}