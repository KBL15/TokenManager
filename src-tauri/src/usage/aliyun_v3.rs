//! 阿里云 OpenAPI V3 签名（ACS3-HMAC-SHA256）。
//!
//! 官方文档：<https://help.aliyun.com/zh/sdk/product-overview/v3-request-structure-and-signature>
//!
//! 百炼的 Token Plan 接口（产品 `modelstudio`，版本 `2026-02-10`）用的是这一套，
//! 和 [`super::aliyun_sign`] 里的 V2 差别很大：
//!
//! | | V2 | V3 |
//! |---|---|---|
//! | 算法 | HMAC-SHA1 | HMAC-SHA256 |
//! | 密钥 | `AccessKeySecret + "&"` | AccessKeySecret 原值 |
//! | 输出 | Base64 | **小写 Hex** |
//! | 参数位置 | URL query | Header `x-acs-*` |
//! | Body | 不参与签名 | **参与**（`x-acs-content-sha256`）|
//!
//! 官方没有 Rust SDK，这里手写。签名逻辑是纯函数，可以单测。

use hmac::{Hmac, Mac as _};
use sha2::{Digest as _, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// 空 body 的 SHA256，是个固定常量。用它做自检能快速发现实现错误。
pub const EMPTY_BODY_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

pub const SIGNATURE_ALGORITHM: &str = "ACS3-HMAC-SHA256";

/// 一次 V3 请求的完整描述。
#[derive(Debug, Clone)]
pub struct V3Request {
    pub method: String,
    /// 主机名，不含协议，如 `modelstudio.cn-beijing.aliyuncs.com`
    pub host: String,
    /// 资源路径，如 `/tokenplan/invite/config/set`
    pub path: String,
    /// 查询参数。会按参数名升序参与签名。
    pub query: Vec<(String, String)>,
    /// 参与签名的头。只有 `host`、`content-type`、`x-acs-*` 会被采纳。
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex_lower(&h.finalize())
}

fn hmac_sha256_hex(key: &[u8], msg: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC 接受任意长度密钥");
    mac.update(msg);
    hex_lower(&mac.finalize().into_bytes())
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0F) as usize] as char);
    }
    s
}

/// 规范查询串：按参数名升序、RFC3986 编码、`&` 连接。
///
/// 排序用原始参数名（不是编码后的），大小写敏感。空参数列表返回空串。
pub fn canonical_query_string(query: &[(String, String)]) -> String {
    let mut items: Vec<(&String, &String)> = query.iter().map(|(k, v)| (k, v)).collect();
    items.sort_by(|a, b| a.0.cmp(b.0));
    items
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                super::aliyun_sign::percent_encode(k),
                super::aliyun_sign::percent_encode(v)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// 判断某个头是否该参与签名。
///
/// 规范只纳入 `host`、`content-type` 和 `x-acs-*`。把 `authorization` 之类也塞进去
/// 会算出错误的签名。
fn is_signable_header(name_lower: &str) -> bool {
    name_lower == "host" || name_lower == "content-type" || name_lower.starts_with("x-acs-")
}

/// 规范头串与 SignedHeaders。
///
/// 返回 `(canonical_headers, signed_headers)`，其中 `canonical_headers` 每行以 `\n`
/// 结尾（含最后一行）。头名小写、按升序、值去首尾空白、重复头名取第一个。
pub fn canonical_headers(headers: &[(String, String)]) -> (String, String) {
    let mut kept: Vec<(String, String)> = Vec::new();

    for (k, v) in headers {
        let kl = k.trim().to_ascii_lowercase();
        if !is_signable_header(&kl) {
            continue;
        }
        // 重复头名只取第一个，避免出现两次
        if kept.iter().any(|(ek, _)| *ek == kl) {
            continue;
        }
        kept.push((kl, v.trim().to_string()));
    }

    kept.sort_by(|a, b| a.0.cmp(&b.0));

    let canonical: String = kept
        .iter()
        .map(|(k, v)| format!("{k}:{v}\n"))
        .collect();
    let signed = kept
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");

    (canonical, signed)
}

/// 规范 URI。
///
/// ROA 风格下是资源路径本身，需按 RFC3986 逐段编码，但**保留 `/` 分隔符**
/// （`/` 编码成 `%2F` 会让路由匹配不上）。
pub fn canonical_uri(path: &str) -> String {
    let p = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    p.split('/')
        .map(super::aliyun_sign::percent_encode)
        .collect::<Vec<_>>()
        .join("/")
}

/// 拼规范请求串（CanonicalRequest）。
///
/// 注意 `canonical_headers` 自身已以 `\n` 结尾，公式里再加一个 `\n`，
/// 于是规范请求串里头与 SignedHeaders 之间**会有一个空行**——这是规范要求的，
/// 不是笔误。
pub fn canonical_request(req: &V3Request) -> String {
    let (ch, sh) = canonical_headers(&req.headers);
    let payload_hash = sha256_hex(&req.body);

    format!(
        "{method}\n{uri}\n{query}\n{headers}\n{signed}\n{payload}",
        method = req.method.to_ascii_uppercase(),
        uri = canonical_uri(&req.path),
        query = canonical_query_string(&req.query),
        headers = ch,
        signed = sh,
        payload = payload_hash,
    )
}

/// 待签名字符串：`ACS3-HMAC-SHA256\n<hex(sha256(CanonicalRequest))>`
pub fn string_to_sign(req: &V3Request) -> String {
    format!(
        "{SIGNATURE_ALGORITHM}\n{}",
        sha256_hex(canonical_request(req).as_bytes())
    )
}

/// 计算签名：`HexEncode(HMAC_SHA256(key = AccessKeySecret, msg = StringToSign))`。
///
/// 注意密钥**不带** V2 那个尾部 `&`。
pub fn compute_signature(req: &V3Request, access_key_secret: &str) -> String {
    hmac_sha256_hex(
        access_key_secret.as_bytes(),
        string_to_sign(req).as_bytes(),
    )
}

/// 拼 `Authorization` 头的值。
pub fn authorization_header(req: &V3Request, access_key_id: &str, access_key_secret: &str) -> String {
    let (_, signed_headers) = canonical_headers(&req.headers);
    let sig = compute_signature(req, access_key_secret);
    format!(
        "{SIGNATURE_ALGORITHM} Credential={access_key_id},SignedHeaders={signed_headers},Signature={sig}"
    )
}

/// 构造带公共 `x-acs-*` 头的完整请求。
///
/// `date` 与 `nonce` 由调用方注入以保持可测试性；生产用 [`now_iso8601`] 和
/// [`new_nonce`]。
#[allow(clippy::too_many_arguments)]
pub fn build_request(
    method: &str,
    host: &str,
    path: &str,
    action: &str,
    version: &str,
    date: &str,
    nonce: &str,
    query: Vec<(String, String)>,
    body: Vec<u8>,
) -> V3Request {
    let content_sha = sha256_hex(&body);
    V3Request {
        method: method.to_string(),
        host: host.to_string(),
        path: path.to_string(),
        query,
        headers: vec![
            ("host".into(), host.to_string()),
            ("x-acs-action".into(), action.to_string()),
            ("x-acs-version".into(), version.to_string()),
            ("x-acs-date".into(), date.to_string()),
            ("x-acs-signature-nonce".into(), nonce.to_string()),
            ("x-acs-content-sha256".into(), content_sha),
        ],
        body,
    }
}

/// `x-acs-date` 要求 ISO8601 UTC，且与服务器时间相差不超过 15 分钟。
pub fn now_iso8601() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

pub fn new_nonce() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// 把签名结果应用到实际发出去的请求头列表上。
///
/// 返回可直接塞进 HTTP 客户端的头。不含 `host`（由客户端自己加）。
pub fn signed_headers(req: &V3Request, access_key_id: &str, access_key_secret: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = req
        .headers
        .iter()
        .filter(|(k, _)| !k.eq_ignore_ascii_case("host"))
        .cloned()
        .collect();

    out.push((
        "authorization".into(),
        authorization_header(req, access_key_id, access_key_secret),
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_req() -> V3Request {
        build_request(
            "POST",
            "modelstudio.cn-beijing.aliyuncs.com",
            "/tokenplan/invite/config/set",
            "SetTokenPlanOrgInviteConfig",
            "2026-02-10",
            "2026-09-27T12:00:00Z",
            "fixednonce",
            vec![
                ("DefaultRoleId".into(), "ORG_MEMBER".into()),
                ("SeatAssignStrategy".into(), "HIGH_TO_LOW".into()),
            ],
            Vec::new(),
        )
    }

    /// 空 body 的哈希是固定常量，对不上说明 SHA256 实现有问题。
    #[test]
    fn empty_body_hash_is_the_known_constant() {
        assert_eq!(sha256_hex(b""), EMPTY_BODY_SHA256);
    }

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn query_string_is_sorted_and_encoded() {
        let q = vec![
            ("b".into(), "2".into()),
            ("a".into(), "1".into()),
            ("c".into(), "x y".into()),
        ];
        // 按名升序；空格编码成 %20
        assert_eq!(canonical_query_string(&q), "a=1&b=2&c=x%20y");
    }

    #[test]
    fn empty_query_gives_empty_string() {
        assert_eq!(canonical_query_string(&[]), "");
    }

    /// 只有 host / content-type / x-acs-* 该参与签名。
    #[test]
    fn only_allowed_headers_are_signed() {
        let headers = vec![
            ("Host".into(), "example.com".into()),
            ("Content-Type".into(), "application/json".into()),
            ("X-Acs-Action".into(), "DoThing".into()),
            ("Authorization".into(), "should-be-ignored".into()),
            ("User-Agent".into(), "should-be-ignored".into()),
            ("Cookie".into(), "should-be-ignored".into()),
        ];

        let (canonical, signed) = canonical_headers(&headers);

        assert!(!canonical.contains("authorization"), "{canonical}");
        assert!(!canonical.contains("user-agent"), "{canonical}");
        assert!(!canonical.contains("cookie"), "{canonical}");
        assert_eq!(
            signed,
            "content-type;host;x-acs-action",
            "头名要小写并按升序"
        );
    }

    /// 每行以 \n 结尾（含最后一行），规范要求如此。
    #[test]
    fn canonical_headers_end_with_newline() {
        let (canonical, _) = canonical_headers(&[("host".into(), "h".into())]);
        assert_eq!(canonical, "host:h\n");
    }

    #[test]
    fn header_values_are_trimmed() {
        let (canonical, _) = canonical_headers(&[("host".into(), "  h  ".into())]);
        assert_eq!(canonical, "host:h\n");
    }

    #[test]
    fn duplicate_headers_keep_first() {
        let (canonical, signed) = canonical_headers(&[
            ("host".into(), "first".into()),
            ("host".into(), "second".into()),
        ]);
        assert_eq!(canonical, "host:first\n");
        assert_eq!(signed, "host");
    }

    /// 路径里的 `/` 必须保留，编码成 %2F 会让路由匹配不上。
    #[test]
    fn canonical_uri_keeps_slashes() {
        assert_eq!(
            canonical_uri("/tokenplan/invite/config/set"),
            "/tokenplan/invite/config/set"
        );
        // 缺前导斜杠要补上
        assert_eq!(canonical_uri("a/b"), "/a/b");
        // 段内特殊字符要编码
        assert_eq!(canonical_uri("/a b/c"), "/a%20b/c");
    }

    /// 规范请求串里，头与 SignedHeaders 之间必须有一个空行。
    #[test]
    fn canonical_request_has_blank_line_before_signed_headers() {
        let cr = canonical_request(&sample_req());
        let lines: Vec<&str> = cr.split('\n').collect();

        assert_eq!(lines[0], "POST");
        assert_eq!(lines[1], "/tokenplan/invite/config/set");
        assert_eq!(lines[2], "DefaultRoleId=ORG_MEMBER&SeatAssignStrategy=HIGH_TO_LOW");

        // 找到 SignedHeaders 那一行，它前面必须是一个空行
        let signed_idx = lines
            .iter()
            .position(|l| l.starts_with("host;x-acs-action"))
            .expect("应有 SignedHeaders 行");
        assert_eq!(lines[signed_idx - 1], "", "SignedHeaders 前应是空行");

        // 最后一行是 payload 哈希
        assert_eq!(*lines.last().unwrap(), EMPTY_BODY_SHA256);
    }

    #[test]
    fn signed_headers_list_is_ascending_and_lowercase() {
        let cr = canonical_request(&sample_req());
        let signed = cr
            .split('\n')
            .find(|l| l.starts_with("host;"))
            .expect("应有 SignedHeaders");
        assert_eq!(
            signed,
            "host;x-acs-action;x-acs-content-sha256;x-acs-date;x-acs-signature-nonce;x-acs-version"
        );
    }

    #[test]
    fn string_to_sign_has_algorithm_prefix_and_hex_digest() {
        let sts = string_to_sign(&sample_req());
        let mut parts = sts.split('\n');
        assert_eq!(parts.next().unwrap(), "ACS3-HMAC-SHA256");

        let digest = parts.next().unwrap();
        assert_eq!(digest.len(), 64, "SHA256 的 hex 应是 64 字符");
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "必须是**小写** hex: {digest}");
    }

    /// V3 的输出是 hex，不是 base64；密钥不带 V2 的尾部 `&`。
    #[test]
    fn signature_is_lowercase_hex() {
        let sig = compute_signature(&sample_req(), "testsecret");
        assert_eq!(sig.len(), 64);
        assert!(
            sig.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "应是小写 hex（V2 才是 base64）: {sig}"
        );
        assert!(!sig.contains('='), "hex 不该有 base64 的填充符: {sig}");
    }

    #[test]
    fn signature_is_deterministic_and_key_sensitive() {
        let req = sample_req();
        assert_eq!(
            compute_signature(&req, "k1"),
            compute_signature(&req, "k1"),
            "同输入应同输出"
        );
        assert_ne!(
            compute_signature(&req, "k1"),
            compute_signature(&req, "k2"),
            "换密钥必须换签名"
        );
    }

    #[test]
    fn changing_body_changes_signature() {
        // V3 把 body 纳入签名，这是与 V2 的关键差异
        let a = sample_req();
        let mut b = sample_req();
        b.body = b"{\"x\":1}".to_vec();

        assert_ne!(compute_signature(&a, "k"), compute_signature(&b, "k"));
    }

    #[test]
    fn changing_nonce_changes_signature() {
        let a = sample_req();
        let mut b = sample_req();
        b.headers = b
            .headers
            .into_iter()
            .map(|(k, v)| {
                if k == "x-acs-signature-nonce" {
                    (k, "other".to_string())
                } else {
                    (k, v)
                }
            })
            .collect();
        assert_ne!(compute_signature(&a, "k"), compute_signature(&b, "k"));
    }

    #[test]
    fn authorization_header_shape() {
        let auth = authorization_header(&sample_req(), "LTAIxxx", "secret");
        assert!(auth.starts_with("ACS3-HMAC-SHA256 Credential=LTAIxxx,SignedHeaders=host;"));
        assert!(auth.contains(",Signature="));
        assert_eq!(auth.matches("Signature=").count(), 1);
    }

    #[test]
    fn signed_headers_omit_host_but_include_authorization() {
        // host 由 HTTP 客户端自己加，不该重复塞一遍
        let hs = signed_headers(&sample_req(), "ak", "sk");
        assert!(!hs.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")));
        assert!(hs.iter().any(|(k, _)| k == "authorization"));
        assert!(hs.iter().any(|(k, _)| k == "x-acs-action"));
    }

    #[test]
    fn build_request_computes_content_sha() {
        let r = build_request(
            "POST", "h", "/p", "A", "2026-02-10", "d", "n", vec![], b"body".to_vec(),
        );
        let expected = sha256_hex(b"body");
        assert!(r
            .headers
            .iter()
            .any(|(k, v)| k == "x-acs-content-sha256" && *v == expected));

        // 空 body 时用固定常量
        let empty = build_request(
            "POST", "h", "/p", "A", "2026-02-10", "d", "n", vec![], Vec::new(),
        );
        assert!(empty
            .headers
            .iter()
            .any(|(k, v)| k == "x-acs-content-sha256" && v == EMPTY_BODY_SHA256));
    }

    #[test]
    fn nonce_is_unique_per_call() {
        let a = new_nonce();
        let b = new_nonce();
        assert_ne!(a, b, "签名随机数每次必须不同");
    }

    #[test]
    fn timestamp_is_utc_iso8601() {
        let ts = now_iso8601();
        assert!(ts.ends_with('Z'), "必须是 UTC: {ts}");
        assert_eq!(ts.len(), 20, "格式应为 yyyy-MM-ddTHH:mm:ssZ: {ts}");
    }
}