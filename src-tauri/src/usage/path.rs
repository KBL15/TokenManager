//! 字段路径解析：从接口返回的 JSON 里按路径取值。
//!
//! 支持形如 `data.quota.remain`、`items[0].used`、`a.b[0].c[1]` 的路径。
//! 路径由设置页的字段探测界面**点选生成**，所以不需要完整的 JSONPath 表达力，
//! 自己实现约 40 行即可，省掉一个依赖。
//!
//! 宽容度是刻意做高的：第三方接口的数据格式千奇百怪，取值失败应该退化成
//! "这个字段没拿到"，而不是让整次刷新崩掉。

use chrono::{DateTime, TimeZone as _, Utc};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seg {
    Key(String),
    Index(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    Empty,
    /// `[` 没闭合，或下标不是数字
    BadIndex(String),
    /// 路径里出现空段，例如 `a..b`
    EmptySegment,
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathError::Empty => write!(f, "路径为空"),
            PathError::BadIndex(s) => write!(f, "下标写法有误: {s}"),
            PathError::EmptySegment => write!(f, "路径里有空的段（是不是多打了个点？）"),
        }
    }
}

impl std::error::Error for PathError {}

/// 把路径字符串拆成段。
///
/// 开头的 `$` 或 `$.` 会被忽略，方便从 JSONPath 工具里直接复制过来。
pub fn parse_path(path: &str) -> Result<Vec<Seg>, PathError> {
    let path = path.trim();
    let path = path.strip_prefix("$.").or_else(|| path.strip_prefix('$')).unwrap_or(path);

    if path.is_empty() {
        return Err(PathError::Empty);
    }

    let mut segs = Vec::new();

    for part in path.split('.') {
        if part.is_empty() {
            return Err(PathError::EmptySegment);
        }

        // 段形如 `name` 或 `name[0][1]`
        let (name, mut rest) = match part.find('[') {
            Some(i) => (&part[..i], &part[i..]),
            None => (part, ""),
        };

        if !name.is_empty() {
            segs.push(Seg::Key(name.to_string()));
        }

        while !rest.is_empty() {
            let close = rest.find(']').ok_or_else(|| PathError::BadIndex(rest.to_string()))?;
            let idx_str = &rest[1..close];
            let idx = idx_str
                .parse::<usize>()
                .map_err(|_| PathError::BadIndex(rest[..=close].to_string()))?;
            segs.push(Seg::Index(idx));
            rest = &rest[close + 1..];
        }
    }

    if segs.is_empty() {
        return Err(PathError::Empty);
    }
    Ok(segs)
}

/// 按路径取值。任一段走不通就返回 `None`（不报错）。
pub fn resolve<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let segs = parse_path(path).ok()?;
    let mut cur = root;

    for seg in &segs {
        cur = match seg {
            Seg::Key(k) => cur.get(k)?,
            Seg::Index(i) => cur.get(i)?,
        };
    }
    Some(cur)
}

/// 取一个数值。
///
/// **刻意同时接受数字和数字字符串**：阿里云系接口大量使用
/// `"AvailableAmount": "10000.00"` 这种字符串形式返回数值，
/// 只认 JSON number 的话会大面积取不到值。
pub fn resolve_number(root: &Value, path: &str) -> Option<f64> {
    let v = resolve(root, path)?;
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// 取一个时间点。兼容三种常见写法：
/// - RFC3339 / ISO8601 字符串，如 `2026-10-01T00:00:00Z`
/// - Unix 秒级时间戳
/// - Unix 毫秒级时间戳（按量级自动判断）
pub fn resolve_datetime(root: &Value, path: &str) -> Option<DateTime<Utc>> {
    let v = resolve(root, path)?;

    match v {
        Value::String(s) => {
            let s = s.trim();
            if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
                return Some(dt.with_timezone(&Utc));
            }
            // 有些接口给的是不带时区的本地时间串，或 "2026-10-01 00:00:00"
            for fmt in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d"] {
                if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
                    return Some(Utc.from_utc_datetime(&naive));
                }
                if let Ok(d) = chrono::NaiveDate::parse_from_str(s, fmt) {
                    return Some(Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0)?));
                }
            }
            // 字符串形式的数字时间戳
            s.parse::<i64>().ok().and_then(epoch_to_datetime)
        }
        Value::Number(n) => n.as_i64().and_then(epoch_to_datetime),
        _ => None,
    }
}

/// 按量级判断是秒还是毫秒。1e12 秒约合公元 33658 年，真实时间戳不会到这个量级。
fn epoch_to_datetime(v: i64) -> Option<DateTime<Utc>> {
    let (secs, nanos) = if v.abs() >= 100_000_000_000 {
        (v.div_euclid(1000), (v.rem_euclid(1000) as u32) * 1_000_000)
    } else {
        (v, 0)
    };
    Utc.timestamp_opt(secs, nanos).single()
}

/// 从「已用量 + 总额度」算出剩余百分比，范围 0–100。
///
/// 用于接口只给了绝对数值、没给百分比的情况。
pub fn percent_from_used_total(used: f64, total: f64) -> Option<f64> {
    if total <= 0.0 {
        return None;
    }
    Some(((total - used) / total * 100.0).clamp(0.0, 100.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_simple_and_nested_paths() {
        assert_eq!(parse_path("a").unwrap(), vec![Seg::Key("a".into())]);
        assert_eq!(
            parse_path("a.b.c").unwrap(),
            vec![Seg::Key("a".into()), Seg::Key("b".into()), Seg::Key("c".into())]
        );
        assert_eq!(
            parse_path("items[0].used").unwrap(),
            vec![Seg::Index(0), Seg::Key("used".into())]
                .into_iter()
                .fold(vec![Seg::Key("items".into())], |mut acc, s| {
                    acc.push(s);
                    acc
                })
        );
    }

    #[test]
    fn strips_jsonpath_dollar_prefix() {
        assert_eq!(parse_path("$.a.b").unwrap(), parse_path("a.b").unwrap());
        assert_eq!(parse_path("$a").unwrap(), parse_path("a").unwrap());
    }

    #[test]
    fn parses_multi_index_and_bare_index() {
        assert_eq!(
            parse_path("a[1][2]").unwrap(),
            vec![Seg::Key("a".into()), Seg::Index(1), Seg::Index(2)]
        );
        assert_eq!(parse_path("[0]").unwrap(), vec![Seg::Index(0)]);
    }

    #[test]
    fn rejects_bad_paths() {
        assert_eq!(parse_path(""), Err(PathError::Empty));
        assert_eq!(parse_path("   "), Err(PathError::Empty));
        assert_eq!(parse_path("a..b"), Err(PathError::EmptySegment));
        assert!(matches!(parse_path("a[0"), Err(PathError::BadIndex(_))));
        assert!(matches!(parse_path("a[x]"), Err(PathError::BadIndex(_))));
    }

    #[test]
    fn resolves_nested_values() {
        let v = json!({
            "data": {
                "quota": { "remain_percent": 62.5 },
                "items": [ { "used": 100 }, { "used": 200 } ]
            }
        });

        assert_eq!(resolve(&v, "data.quota.remain_percent").unwrap(), &json!(62.5));
        assert_eq!(resolve(&v, "data.items[1].used").unwrap(), &json!(200));
    }

    #[test]
    fn resolve_returns_none_instead_of_panicking() {
        let v = json!({ "a": 1 });
        assert!(resolve(&v, "a.b").is_none());
        assert!(resolve(&v, "nope").is_none());
        assert!(resolve(&v, "a[5]").is_none());
        assert!(resolve(&v, "a[0]").is_none());
        assert!(resolve(&v, "bad..path").is_none());
    }

    /// 阿里云系接口把数字当字符串返回，这条不兼容的话会大面积取不到值。
    #[test]
    fn number_accepts_string_encoded_numbers() {
        let v = json!({
            "num": 42,
            "str": "10000.00",
            "with_space": "  7.5  ",
            "bad": "abc",
            "bool": true
        });

        assert_eq!(resolve_number(&v, "num"), Some(42.0));
        assert_eq!(resolve_number(&v, "str"), Some(10000.0));
        assert_eq!(resolve_number(&v, "with_space"), Some(7.5));
        assert_eq!(resolve_number(&v, "bad"), None);
        assert_eq!(resolve_number(&v, "bool"), None);
    }

    #[test]
    fn datetime_accepts_rfc3339_and_epochs() {
        // 2026-10-01T00:00:00Z == 1790812800 秒
        let v = json!({
            "rfc": "2026-10-01T00:00:00Z",
            "offset": "2026-10-01T08:00:00+08:00",
            "secs": 1790812800,
            "millis": 1790812800000i64,
            "str_secs": "1790812800",
            "local": "2026-10-01 00:00:00"
        });

        let rfc = resolve_datetime(&v, "rfc").unwrap();
        assert_eq!(rfc.to_rfc3339(), "2026-10-01T00:00:00+00:00");
        // 带时区偏移的应换算成同一时刻
        assert_eq!(resolve_datetime(&v, "offset").unwrap(), rfc);
        // 秒和毫秒应得到同一时刻
        assert_eq!(resolve_datetime(&v, "secs").unwrap(), rfc);
        assert_eq!(resolve_datetime(&v, "millis").unwrap(), rfc);
        assert_eq!(resolve_datetime(&v, "str_secs").unwrap(), rfc);
        assert_eq!(resolve_datetime(&v, "local").unwrap(), rfc);
    }

    #[test]
    fn percent_from_used_total_handles_edges() {
        assert_eq!(percent_from_used_total(250.0, 1000.0), Some(75.0));
        assert_eq!(percent_from_used_total(0.0, 1000.0), Some(100.0));
        assert_eq!(percent_from_used_total(1000.0, 1000.0), Some(0.0));
        // 超额不应给出负数
        assert_eq!(percent_from_used_total(1200.0, 1000.0), Some(0.0));
        // 总额为 0 无法计算，避免除零
        assert_eq!(percent_from_used_total(10.0, 0.0), None);
    }
}