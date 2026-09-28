//! 取数失败的类型。
//!
//! 这里刻意做细分类而不是统一成一个字符串，因为**不同失败原因对应完全不同的
//! 用户操作**：登录态失效要去重新粘贴 Cookie，UA 被拒是程序 bug，字段找不到是
//! 接口改版了要去重新探测映射。错误信息必须能直接告诉用户下一步做什么。

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum FetchError {
    /// 还没配置好（缺 URL、缺凭据等）
    NotConfigured { detail: String },

    /// 网络层失败：连不上、超时、DNS
    Network { detail: String },

    /// 登录态失效或凭据无效——**最常见**，需要用户重新获取 Cookie
    Auth { detail: String },

    /// 请求被网关拒绝（如 UA 校验、workspace 越权）
    GatewayRejected { detail: String },

    /// 拿到了响应但解析不出需要的字段——通常是接口改版
    Parse { detail: String },

    /// HTTP 非 2xx
    Http { status: u16, detail: String },
}

impl FetchError {
    /// 给用户看的一句话，含明确的下一步动作。
    pub fn user_message(&self) -> String {
        match self {
            FetchError::NotConfigured { detail } => format!("尚未配置完整：{detail}"),
            FetchError::Network { detail } => format!("网络请求失败：{detail}"),
            FetchError::Auth { detail } => {
                format!("登录态已失效，请在设置页重新粘贴 Cookie。（{detail}）")
            }
            FetchError::GatewayRejected { detail } => format!("请求被网关拒绝：{detail}"),
            FetchError::Parse { detail } => {
                format!("响应解析失败，接口可能已改版，请到设置页重新探测字段。（{detail}）")
            }
            FetchError::Http { status, detail } => format!("服务返回 HTTP {status}：{detail}"),
        }
    }

    /// 是否属于「用户重新操作一下就能好」的错——卡片据此决定要不要弹设置引导。
    pub fn is_user_actionable(&self) -> bool {
        matches!(
            self,
            FetchError::Auth { .. } | FetchError::Parse { .. } | FetchError::NotConfigured { .. }
        )
    }
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.user_message())
    }
}

impl std::error::Error for FetchError {}

/// 把服务端返回的错误码翻译成分类错误。
///
/// 码值来自两个逆向项目实测记录的网关错误：
/// - `NotLogined` —— Cookie 过期或抄错了 host 的 Cookie
/// - `PostonlyOrTokenError` —— UA 不像浏览器，被网关人机校验拦下
/// - `BailianGateway.Workspace.NotAuthorised` —— 请求里混进了别的账号的 switchAgent
pub fn classify_gateway_code(code: &str, message: &str) -> FetchError {
    let detail = if message.is_empty() {
        code.to_string()
    } else {
        format!("{code}: {message}")
    };

    let c = code.to_ascii_lowercase();
    if c.contains("notlogined") || c.contains("notlogin") || c.contains("needlogin") {
        FetchError::Auth { detail }
    } else if c.contains("postonly") || c.contains("token") && c.contains("error") {
        FetchError::GatewayRejected { detail }
    } else if c.contains("workspace") || c.contains("notauthorised") || c.contains("notauthorized") {
        FetchError::GatewayRejected { detail }
    } else {
        FetchError::Parse { detail }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_login_expiry() {
        let e = classify_gateway_code("NotLogined", "");
        assert!(matches!(e, FetchError::Auth { .. }));
        assert!(e.is_user_actionable());
        assert!(e.user_message().contains("重新粘贴 Cookie"), "{}", e.user_message());
    }

    #[test]
    fn classifies_ua_rejection() {
        let e = classify_gateway_code("PostonlyOrTokenError", "bad ua");
        assert!(matches!(e, FetchError::GatewayRejected { .. }));
        // UA 被拒是程序问题，不该让用户去改配置
        assert!(!e.is_user_actionable());
    }

    #[test]
    fn classifies_workspace_denial() {
        let e = classify_gateway_code("BailianGateway.Workspace.NotAuthorised", "denied");
        assert!(matches!(e, FetchError::GatewayRejected { .. }));
    }

    #[test]
    fn parse_error_tells_user_to_reprobe() {
        let e = classify_gateway_code("SomethingNew", "unknown");
        assert!(matches!(e, FetchError::Parse { .. }));
        assert!(e.user_message().contains("重新探测字段"), "{}", e.user_message());
    }

    #[test]
    fn actionable_flag_matches_user_workflow() {
        assert!(FetchError::Auth { detail: "x".into() }.is_user_actionable());
        assert!(FetchError::Parse { detail: "x".into() }.is_user_actionable());
        assert!(FetchError::NotConfigured { detail: "x".into() }.is_user_actionable());
        assert!(!FetchError::Network { detail: "x".into() }.is_user_actionable());
        assert!(!FetchError::Http { status: 500, detail: "x".into() }.is_user_actionable());
    }
}