//! 敏感凭据的存储。
//!
//! 走 Windows 凭据管理器（通过 `keyring`），**不落配置文件**。
//! 配置文件里只保留非敏感的 AccessKey ID、URL 等；Cookie、Secret、API Key 全在这里。
//!
//! `keyring` 4.x 的 API 在 `keyring::v1` 子模块下（4.0 是破坏性大版本，
//! `keyring::Entry` 那个路径是 3.x 的写法）。
//!
//! 凭据是 **per-Windows-用户** 的：换个 Windows 账号登录就读不到，这是预期行为。
//!
//! # 为什么要分片
//!
//! Windows 凭据管理器单条凭据的上限是 **2560 个 UTF-16 字符**，超了会直接报错：
//!
//! ```text
//! Value of 'password encoded as UTF-16' is longer than the platform limit of 2560 chars
//! ```
//!
//! AccessKey Secret 只有 30 来个字符，从来不会碰到这个限制；但**阿里云控制台的
//! Cookie 动辄 2–5 KB**，必然超限。所以这里把长值切成多片，用一条 `count` 记录
//! 片数，读的时候拼回去。

use keyring::v1::Entry;

/// 服务名。会显示在凭据管理器里，用有辨识度的域名式命名，别用 "test"。
pub const SERVICE: &str = "com.tokenmoniter.app";

/// 控制台网关的整行 Cookie
pub const KEY_COOKIE: &str = "bailian-cookie";
/// 阿里云 AccessKey Secret（AccessKey ID 不敏感，存配置里）
pub const KEY_AK_SECRET: &str = "aliyun-accesskey-secret";
/// 自定义接口的凭据
pub const KEY_GENERIC: &str = "generic-credential";

/// 所有需要管理的凭据键
pub const ALL_KEYS: [&str; 3] = [KEY_COOKIE, KEY_AK_SECRET, KEY_GENERIC];

/// 单片的字符数上限。
///
/// 平台限制是 2560，这里取 1000，留足余量。**按字符而不是字节切**，
/// 避免把多字节字符（中文、百分号转义会有大量 ASCII，但字段值可能有中文）切坏。
const MAX_CHUNK_CHARS: usize = 1000;

/// 清理残留分片时扫过的片数上限。
///
/// 一次写入中途失败可能留下孤儿分片，而那时 `count` 已经不在了、无从得知原有片数。
/// 1000 字符 × 64 片 = 64000 字符，远超任何真实 Cookie，扫这么多足够且代价可控。
const SWEEP_CHUNKS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretError {
    /// 当前平台没有可用的凭据存储
    StoreUnavailable(String),
    /// 其他失败
    Failed(String),
}

impl std::fmt::Display for SecretError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecretError::StoreUnavailable(d) => write!(f, "系统凭据管理器不可用：{d}"),
            SecretError::Failed(d) => write!(f, "凭据操作失败：{d}"),
        }
    }
}

impl std::error::Error for SecretError {}

fn entry(key: &str) -> Result<Entry, SecretError> {
    Entry::new(SERVICE, key).map_err(|e| SecretError::Failed(e.to_string()))
}

fn chunk_key(key: &str, i: usize) -> String {
    format!("{key}.{i}")
}

fn count_key(key: &str) -> String {
    format!("{key}.count")
}

fn read_entry(key: &str) -> Option<String> {
    let e = entry(key).ok()?;
    match e.get_password() {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => None,
    }
}

fn write_entry(key: &str, value: &str) -> Result<(), SecretError> {
    entry(key)?
        .set_password(value)
        .map_err(|e| SecretError::Failed(e.to_string()))
}

fn delete_entry(key: &str) -> Result<(), SecretError> {
    match entry(key)?.delete_credential() {
        Ok(()) => Ok(()),
        // 本来就不存在，不算错（幂等）
        Err(keyring::v1::Error::NoEntry) => Ok(()),
        Err(e) => Err(SecretError::Failed(e.to_string())),
    }
}

fn read_count(key: &str) -> Option<usize> {
    read_entry(&count_key(key))?.trim().parse().ok()
}

/// 写入凭据。超长值自动分片。
///
/// 空值等同于删除——避免把空字符串当凭据存进去，那样之后读出来会误以为"已配置"。
pub fn set(key: &str, value: &str) -> Result<(), SecretError> {
    if value.trim().is_empty() {
        return delete(key);
    }

    let chars: Vec<char> = value.chars().collect();
    let chunks: Vec<String> = chars
        .chunks(MAX_CHUNK_CHARS)
        .map(|c| c.iter().collect())
        .collect();

    let old_count = read_count(key).unwrap_or(0);

    // ⚠️ 顺序是这段代码的关键：先让 count 消失。
    // 这样即使写到一半失败，读出来也是"没有凭据"，而不是新旧分片拼在一起的垃圾值。
    // 宁可让用户重填一次，也不能拿一个错误的 Cookie 去请求。
    delete_entry(&count_key(key))?;
    // 清掉可能存在的旧格式整条值
    delete_entry(key)?;

    for (i, part) in chunks.iter().enumerate() {
        write_entry(&chunk_key(key, i), part)?;
    }

    // 清掉上一轮多出来的分片
    for i in chunks.len()..old_count.max(chunks.len()) {
        let _ = delete_entry(&chunk_key(key, i));
    }

    // 最后写 count，它相当于这次写入的"提交点"
    write_entry(&count_key(key), &chunks.len().to_string())
}

/// 读取凭据。读不到返回 `None`（**不报错**）——没配过是正常状态。
pub fn get(key: &str) -> Option<String> {
    // 新格式：按分片拼回
    if let Some(n) = read_count(key) {
        let mut out = String::new();
        for i in 0..n {
            // 任一片缺失就认为整体无效，不要返回残缺的凭据
            out.push_str(&read_entry(&chunk_key(key, i))?);
        }
        return if out.trim().is_empty() { None } else { Some(out) };
    }

    // 兼容早期写入的未分片值
    read_entry(key)
}

/// 删除凭据。键不存在也算成功（幂等）。
pub fn delete(key: &str) -> Result<(), SecretError> {
    let n = read_count(key).unwrap_or(0);

    // 同样先删 count，避免读到半删状态
    delete_entry(&count_key(key))?;

    // 知道片数就按片数删；不知道（count 已丢，可能来自写入中断）
    // 就扫一遍上限范围，把孤儿分片清干净
    let sweep = if n == 0 { SWEEP_CHUNKS } else { n.min(SWEEP_CHUNKS) };
    for i in 0..sweep {
        let _ = delete_entry(&chunk_key(key, i));
    }

    delete_entry(key)
}

/// 判断凭据存储是否可用。设置页据此提示用户，而不是等到保存时才报错。
pub fn store_available() -> bool {
    entry(KEY_COOKIE).is_ok()
}

/// 载入全部凭据，供刷新时使用。
pub fn load_all() -> crate::usage::provider::Credentials {
    crate::usage::provider::Credentials {
        cookie: get(KEY_COOKIE),
        access_key_secret: get(KEY_AK_SECRET),
        generic: get(KEY_GENERIC),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ⚠️ 每个测试必须用**自己的键**。
    //
    // 凭据管理器是全局 OS 状态，而 cargo 默认并行跑测试；共用一个键会让测试
    // 互相读到对方写的值、或把对方的数据删掉，表现为随机失败。
    // 这些键都以 __tm_test_ 开头，不会和真实配置（bailian-cookie 等）冲突。
    const K_BASIC: &str = "__tm_test_basic__";
    const K_LONG: &str = "__tm_test_long__";
    const K_MULTIBYTE: &str = "__tm_test_multibyte__";
    const K_SHRINK: &str = "__tm_test_shrink__";
    const K_AT_BOUNDARY: &str = "__tm_test_atbound__";
    const K_OVER_BOUNDARY: &str = "__tm_test_overbound__";
    const K_INCOMPLETE: &str = "__tm_test_incomplete__";
    const K_EMPTY: &str = "__tm_test_empty__";

    /// 这些测试操作的是 Windows 凭据管理器——一个**全局的、跨线程共享的 OS 资源**。
    ///
    /// cargo 默认并行跑测试。实测即使每个测试用不同键，并发读写仍会互相干扰
    /// （表现为随机读到本该已被覆盖的旧值）。所以用一把互斥锁把它们串起来。
    static GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        GUARD.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn key_names_are_distinct_and_stable() {
        let _g = lock();
        assert_ne!(KEY_COOKIE, KEY_AK_SECRET);
        assert_ne!(KEY_AK_SECRET, KEY_GENERIC);
        assert_eq!(ALL_KEYS.len(), 3);
        assert!(SERVICE.contains("tokenmoniter"));
    }

    #[test]
    fn chunk_key_format_is_stable() {
        let _g = lock();
        assert_eq!(chunk_key("k", 3), "k.3");
        assert_eq!(count_key("k"), "k.count");
    }

    #[test]
    fn round_trip_short_value() {
        let _g = lock();
        if !store_available() {
            eprintln!("跳过：本机凭据管理器不可用");
            return;
        }

        set(K_BASIC, "hello-secret").expect("写入应成功");
        assert_eq!(get(K_BASIC).as_deref(), Some("hello-secret"));

        delete(K_BASIC).expect("删除应成功");
        assert_eq!(get(K_BASIC), None, "删除后应读不到");
    }

    /// 这条是本次修复的核心：控制台 Cookie 有 2–5 KB，必须能存下。
    #[test]
    fn round_trip_value_exceeding_platform_limit() {
        let _g = lock();
        if !store_available() {
            return;
        }

        // 造一个 4200 字符的假 Cookie，远超 2560 的平台上限
        let long: String = "cna=abcdef; ".repeat(350);
        assert!(long.chars().count() > 2560, "测试值必须真的超限");

        set(K_LONG, &long).expect("超长值应能分片存入");
        let back = get(K_LONG).expect("应能读回");
        assert_eq!(back.len(), long.len(), "长度必须一致");
        assert_eq!(back, long, "内容必须逐字符一致");

        delete(K_LONG).unwrap();
        assert_eq!(get(K_LONG), None);
    }

    /// 分片要按字符切，不能把多字节字符切坏。
    #[test]
    fn round_trip_multibyte_value() {
        let _g = lock();
        if !store_available() {
            return;
        }

        // 3000 个中文字符，正好跨过 2560 上限且是多字节
        let long: String = "测试凭据内容".repeat(500);
        assert!(long.chars().count() > 2560);

        set(K_MULTIBYTE, &long).expect("多字节超长值应能存入");
        assert_eq!(get(K_MULTIBYTE).as_deref(), Some(long.as_str()));

        delete(K_MULTIBYTE).unwrap();
    }

    /// 从长值改成短值，多余的分片必须被清掉，
    /// 否则读回来会是"新值 + 旧残留"。
    #[test]
    fn shrinking_value_clears_stale_chunks() {
        let _g = lock();
        if !store_available() {
            return;
        }

        let long: String = "x".repeat(3000);
        set(K_SHRINK, &long).unwrap();
        assert!(read_count(K_SHRINK).unwrap() >= 3, "长值应产生多片");

        set(K_SHRINK, "short").unwrap();
        assert_eq!(get(K_SHRINK).as_deref(), Some("short"), "缩短后不应残留旧分片");
        assert_eq!(read_count(K_SHRINK), Some(1));

        delete(K_SHRINK).unwrap();
    }

    #[test]
    fn missing_key_returns_none_not_error() {
        let _g = lock();
        if !store_available() {
            return;
        }
        assert_eq!(get("__definitely_not_set__"), None);
    }

    #[test]
    fn delete_is_idempotent() {
        let _g = lock();
        if !store_available() {
            return;
        }
        delete("__not_present_key__").expect("删除不存在的键应视为成功");
        delete("__not_present_key__").expect("重复删除也应成功");
    }

    /// 空值当删除处理，避免"空凭据被当成已配置"。
    #[test]
    fn setting_empty_value_clears_it() {
        let _g = lock();
        if !store_available() {
            return;
        }

        set(K_EMPTY, "something").unwrap();
        assert!(get(K_EMPTY).is_some());

        set(K_EMPTY, "   ").unwrap();
        assert_eq!(get(K_EMPTY), None, "空值应清掉而不是存空串");
    }

    /// 恰好一片的值不应被分片逻辑搞坏。
    #[test]
    fn value_exactly_at_chunk_boundary() {
        let _g = lock();
        if !store_available() {
            return;
        }

        let v: String = "a".repeat(MAX_CHUNK_CHARS);
        set(K_AT_BOUNDARY, &v).unwrap();
        assert_eq!(read_count(K_AT_BOUNDARY), Some(1));
        assert_eq!(get(K_AT_BOUNDARY).as_deref(), Some(v.as_str()));

        delete(K_AT_BOUNDARY).unwrap();
    }

    #[test]
    fn value_one_over_chunk_boundary_splits() {
        let _g = lock();
        if !store_available() {
            return;
        }

        let v: String = "a".repeat(MAX_CHUNK_CHARS + 1);
        set(K_OVER_BOUNDARY, &v).unwrap();
        assert_eq!(read_count(K_OVER_BOUNDARY), Some(2), "超出一片应分成两片");
        assert_eq!(get(K_OVER_BOUNDARY).as_deref(), Some(v.as_str()));

        delete(K_OVER_BOUNDARY).unwrap();
    }

    /// 分片不完整时宁可返回 None，也不能返回残缺的凭据。
    #[test]
    fn incomplete_chunks_yield_none() {
        let _g = lock();
        if !store_available() {
            return;
        }

        let v: String = "a".repeat(2500); // 3 片
        set(K_INCOMPLETE, &v).unwrap();
        assert_eq!(read_count(K_INCOMPLETE), Some(3));

        // 人为删掉中间一片，模拟写入不完整
        delete_entry(&chunk_key(K_INCOMPLETE, 1)).unwrap();

        assert_eq!(get(K_INCOMPLETE), None, "缺片时必须返回 None，不能给出残缺值");

        delete(K_INCOMPLETE).unwrap();
    }
}