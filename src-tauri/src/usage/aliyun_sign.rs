//! 阿里云 OpenAPI V2（RPC 风格）签名。
//!
//! 官方文档：<https://help.aliyun.com/zh/sdk/product-overview/rpc-mechanism/>
//!
//! ⚠️ 别和 OSS(ROA) 的签名搞混。那套的 StringToSign 是一长串
//! `HTTPMethod\nAccept\nContent-MD5\nContent-Type\nDate\nCanonicalizedHeaders\nCanonicalizedResource`；
//! **RPC 风格只有三段**，见 [`string_to_sign`]。
//!
//! 整个模块是纯函数，不碰网络、不读时钟（nonce/时间戳由调用方注入），
//! 这样才可以用官方测试向量做单元测试。

use std::collections::BTreeMap;

use base64::Engine as _;
use hmac::{Hmac, Mac as _};
use sha1::Sha1;

type HmacSha1 = Hmac<Sha1>;

/// 签名算法标识，作为公共参数 `SignatureMethod` 的值。
pub const SIGNATURE_METHOD: &str = "HMAC-SHA1";
/// 签名算法版本，作为公共参数 `SignatureVersion` 的值。
pub const SIGNATURE_VERSION: &str = "1.0";

/// RFC3986 百分号编码。
///
/// 不编码 `A-Za-z0-9` 和 `-_.~`，其余一律编码成 `%XX`（大写十六进制）。
///
/// 有两处**与 Java `URLEncoder` 不同**，而这两处恰好是签名不匹配最常见的原因：
/// - 空格编码为 `%20`，不是 `+`
/// - `~` 保持原样，Java 会编成 `%7E`
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0F) as usize] as char);
            }
        }
    }
    out
}

/// 拼规范查询串（Canonicalized Query String）。
///
/// 参数名按 ASCII 字节序升序排列——`BTreeMap<String, _>` 的迭代顺序正好就是这个，
/// 所以这里直接用 BTreeMap 而不是 Vec，省掉一次显式排序，也杜绝了"忘了排序"这个 bug。
/// 排序**大小写敏感**，按原始参数名排（不是排编码后的名字）。
///
/// 注意 `Signature` 本身不参与签名，调用方不要把它放进 map。
pub fn canonicalized_query_string(params: &BTreeMap<String, String>) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// 拼待签名字符串。RPC 风格只有三段。
///
/// 关键在最后一重的 `percent_encode(cqs)`：它是对**已经编码过的**查询串再编码一次，
/// 于是 `%3A` 变成 `%253A`。漏掉这层二次编码就是 `SignatureDoesNotMatch` 的头号原因。
pub fn string_to_sign(http_method: &str, canonicalized_query: &str) -> String {
    format!(
        "{}&{}&{}",
        http_method,
        percent_encode("/"),
        percent_encode(canonicalized_query)
    )
}

/// 计算签名：`Base64(HMAC-SHA1(key = AccessKeySecret + "&", msg = stringToSign))`。
///
/// 密钥末尾那个 `&` 是协议要求，不是笔误。
pub fn compute_signature(
    http_method: &str,
    params: &BTreeMap<String, String>,
    access_key_secret: &str,
) -> String {
    let cqs = canonicalized_query_string(params);
    let sts = string_to_sign(http_method, &cqs);

    let mut mac = HmacSha1::new_from_slice(format!("{access_key_secret}&").as_bytes())
        .expect("HMAC-SHA1 接受任意长度密钥");
    mac.update(sts.as_bytes());

    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

/// 生成最终要请求的查询串，末尾附上签名。
///
/// 签名结果必须**再编码一次**才能拼进 URL——base64 里含 `+` `/` `=`，
/// 直接拼会破坏查询串结构。
pub fn build_signed_query(
    http_method: &str,
    params: &BTreeMap<String, String>,
    access_key_secret: &str,
) -> String {
    let cqs = canonicalized_query_string(params);
    let sig = compute_signature(http_method, params, access_key_secret);
    format!("{cqs}&Signature={}", percent_encode(&sig))
}

/// 在业务参数之上补齐公共请求参数，返回完整参数集。
///
/// `nonce` 和 `timestamp` 由调用方注入，这样本模块保持纯函数、可测试。
/// 生产调用请用 [`new_nonce`] 和 [`format_timestamp`]。
///
/// 注意 `Format` 必须显式传 `JSON`：**不传的话默认返回 XML**。
pub fn with_common_params(
    action: &str,
    version: &str,
    access_key_id: &str,
    nonce: &str,
    timestamp: &str,
    api_params: impl IntoIterator<Item = (String, String)>,
) -> BTreeMap<String, String> {
    let mut params: BTreeMap<String, String> = api_params.into_iter().collect();

    params.insert("Action".into(), action.into());
    params.insert("Version".into(), version.into());
    params.insert("Format".into(), "JSON".into());
    params.insert("AccessKeyId".into(), access_key_id.into());
    params.insert("SignatureMethod".into(), SIGNATURE_METHOD.into());
    params.insert("SignatureVersion".into(), SIGNATURE_VERSION.into());
    params.insert("SignatureNonce".into(), nonce.into());
    params.insert("Timestamp".into(), timestamp.into());

    params
}

/// 生成签名随机数。协议要求每次请求都不同，重复会报错。
pub fn new_nonce() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// 把时间格式化成协议要求的 UTC 形式：`yyyy-MM-ddTHH:mm:ssZ`。
///
/// 必须是 UTC，不能用本地时间；有效期 31 分钟，注意本机时钟漂移。
pub fn format_timestamp(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 官方文档给出的固定测试向量。
    ///
    /// 来源：<https://help.aliyun.com/zh/sdk/product-overview/rpc-mechanism/>
    /// 用 ECS 的 `DescribeDedicatedHosts` 举例，签名期望 `9NaGiOspFP5UPcwX8Iwt2YJXXuk=`。
    ///
    /// 这个用例是整个数据层的地基——它绿了才说明签名逻辑对。
    fn official_vector_params() -> BTreeMap<String, String> {
        let mut p = BTreeMap::new();
        p.insert("AccessKeyId".into(), "testid".into());
        p.insert("Action".into(), "DescribeDedicatedHosts".into());
        p.insert("Format".into(), "JSON".into());
        p.insert("RegionId".into(), "cn-beijing".into());
        p.insert("SignatureMethod".into(), "HMAC-SHA1".into());
        p.insert(
            "SignatureNonce".into(),
            "edb2b34af0af9a6d14deaf7c1a5315eb".into(),
        );
        p.insert("SignatureVersion".into(), "1.0".into());
        p.insert("Timestamp".into(), "2023-03-13T08:34:30Z".into());
        p.insert("Version".into(), "2014-05-26".into());
        p
    }

    #[test]
    fn cqs_matches_official_vector() {
        let cqs = canonicalized_query_string(&official_vector_params());
        assert_eq!(
            cqs,
            "AccessKeyId=testid&Action=DescribeDedicatedHosts&Format=JSON&RegionId=cn-beijing&\
             SignatureMethod=HMAC-SHA1&SignatureNonce=edb2b34af0af9a6d14deaf7c1a5315eb&\
             SignatureVersion=1.0&Timestamp=2023-03-13T08%3A34%3A30Z&Version=2014-05-26"
                .replace(' ', "")
        );
    }

    #[test]
    fn string_to_sign_double_encodes() {
        let cqs = canonicalized_query_string(&official_vector_params());
        let sts = string_to_sign("GET", &cqs);

        // 前缀固定是 GET&%2F&
        assert!(sts.starts_with("GET&%2F&"), "实际: {}", &sts[..20.min(sts.len())]);

        // 二次编码的核心证据：原来的 %3A 变成了 %253A
        assert!(
            sts.contains("Timestamp%3D2023-03-13T08%253A34%253A30Z"),
            "二次编码没生效: {sts}"
        );
    }

    #[test]
    fn signature_matches_official_vector() {
        assert_eq!(
            compute_signature("GET", &official_vector_params(), "testsecret"),
            "9NaGiOspFP5UPcwX8Iwt2YJXXuk="
        );
    }

    #[test]
    fn percent_encode_rules() {
        // 未保留字符原样
        assert_eq!(percent_encode("abcXYZ019-_.~"), "abcXYZ019-_.~");
        // 空格 → %20，不是 +
        assert_eq!(percent_encode("a b"), "a%20b");
        // * 必须编码
        assert_eq!(percent_encode("*"), "%2A");
        // ~ 必须保持不编码（Java URLEncoder 会错编成 %7E）
        assert_eq!(percent_encode("~"), "~");
        // 冒号、斜杠
        assert_eq!(percent_encode(":"), "%3A");
        assert_eq!(percent_encode("/"), "%2F");
        // 多字节 UTF-8 逐字节编码
        assert_eq!(percent_encode("中"), "%E4%B8%AD");
    }

    #[test]
    fn signed_query_encodes_base64_padding() {
        let q = build_signed_query("GET", &official_vector_params(), "testsecret");
        // 签名末尾的 = 必须被编码，否则会破坏查询串
        assert!(q.ends_with("Signature=9NaGiOspFP5UPcwX8Iwt2YJXXuk%3D"), "实际: {q}");
    }

    #[test]
    fn common_params_are_complete_and_sorted() {
        let p = with_common_params(
            "GetTechSolutionTokenOverview",
            "2017-12-14",
            "my-ak",
            "nonce123",
            "2026-09-27T12:00:00Z",
            [("RegionId".to_string(), "cn-beijing".to_string())],
        );

        for key in [
            "Action",
            "Version",
            "Format",
            "AccessKeyId",
            "SignatureMethod",
            "SignatureVersion",
            "SignatureNonce",
            "Timestamp",
            "RegionId",
        ] {
            assert!(p.contains_key(key), "缺公共参数 {key}");
        }

        // BTreeMap 的迭代顺序即签名用的顺序，必须是 ASCII 升序
        let keys: Vec<_> = p.keys().cloned().collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);

        // Format 不显式传 JSON 的话阿里云默认返回 XML
        assert_eq!(p.get("Format").unwrap(), "JSON");
    }
}