//! 系统托盘。
//!
//! 图标上的百分比由 [`crate::icon`] 直接画进位图——不用 `set_title`，
//! 因为 Windows 上那是把文字挂在图标**旁边**，样式不可控还会撑开通知区域。

use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Runtime};

use crate::config::AppConfig;
use crate::icon;
use crate::usage::UsageSnapshot;

pub const TRAY_ID: &str = "main-tray";

/// 把快照渲染成托盘悬停提示。
pub fn tooltip(snap: &UsageSnapshot, source_label: &str) -> String {
    if let Some(err) = &snap.error {
        if !snap.has_data() {
            return format!("TokenMonitor · {err}");
        }
    }

    let mut lines = vec!["TokenMonitor".to_string(), source_label.to_string()];

    // 单位前留一个空格，"12400 Credits" 而不是 "12400Credits"
    let unit = snap.unit.as_deref().unwrap_or("").trim();
    let unit_suffix = if unit.is_empty() {
        String::new()
    } else {
        format!(" {unit}")
    };

    for w in &snap.windows {
        let pct = w
            .effective_remaining_pct()
            .map(|p| format!("{p:.0}%"))
            .unwrap_or_else(|| "—".into());
        match w.remaining_amount() {
            // 有绝对量就一并显示，比光一个百分比更有用。
            // 保留两位小数，和卡片上的显示保持一致
            Some(rem) => lines.push(format!("{}：剩余 {rem:.2}{unit_suffix}（{pct}）", w.label)),
            None => lines.push(format!("{}：剩余 {pct}", w.label)),
        }
    }

    if let Some(w) = snap.windows.first() {
        if let Some(reset) = w.reset_at {
            lines.push(format!("重置：{}", reset.with_timezone(&chrono::Local).format("%m-%d %H:%M")));
        }
    }

    if snap.error.is_some() {
        lines.push("⚠ 数据可能已过期".into());
    }

    lines.join("\n")
}

/// 创建托盘。必须在 setup 里调用一次。
pub fn setup<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let show_card = MenuItem::with_id(app, "show-card", "显示/隐藏卡片", true, None::<&str>)?;
    let refresh = MenuItem::with_id(app, "refresh", "立即刷新", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "设置…", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;

    let menu = Menu::with_items(app, &[&show_card, &refresh, &settings, &sep, &quit])?;

    let initial = Image::new_owned(icon::render_icon(None), icon::SIZE, icon::SIZE);

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(initial)
        .tooltip("TokenMonitor")
        .menu(&menu)
        // 左键点击切换卡片显隐，所以菜单只在右键弹
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show-card" => crate::toggle_card(app),
            "refresh" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    crate::refresh_and_broadcast(app).await;
                });
            }
            "settings" => crate::show_settings(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                crate::toggle_card(tray.app_handle());
            }
        })
        .build(app)?;

    Ok(())
}

/// 按最新快照更新托盘图标与提示。
pub fn update<R: Runtime>(app: &AppHandle<R>, snap: &UsageSnapshot, cfg: &AppConfig) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };

    let pct = snap.most_constrained().and_then(|w| w.effective_remaining_pct());

    // 托盘显示百分比可以在设置里关掉
    let image = if cfg.tray_shows_percentage {
        Image::new_owned(icon::render_icon(pct), icon::SIZE, icon::SIZE)
    } else {
        Image::new_owned(icon::render_icon(None), icon::SIZE, icon::SIZE)
    };
    let _ = tray.set_icon(Some(image));

    let _ = tray.set_tooltip(Some(tooltip(snap, &snap.source_label)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{UsageSnapshot, UsageWindow};
    use chrono::Utc;

    fn snap_with(pct: Option<f64>, err: Option<&str>) -> UsageSnapshot {
        let mut w = UsageWindow::new("本月");
        w.remaining_pct = pct;
        w.total = Some(20000.0);
        w.used = Some(7600.0);

        UsageSnapshot {
            windows: vec![w],
            unit: Some("Credits".into()),
            fetched_at: Utc::now(),
            source_label: "百炼 Token Plan".into(),
            error: err.map(String::from),
            last_success: if err.is_some() { None } else { Some(Utc::now()) },
        }
    }

    #[test]
    fn tooltip_shows_label_and_amount() {
        let t = tooltip(&snap_with(Some(62.0), None), "百炼 Token Plan");
        assert!(t.contains("百炼 Token Plan"), "{t}");
        assert!(t.contains("本月"), "{t}");
        assert!(t.contains("62%"), "{t}");
        assert!(t.contains("12400"), "应显示剩余绝对量: {t}");
    }

    /// 取数彻底失败时，提示第一条就是错误原因，用户一眼能看到。
    #[test]
    fn tooltip_leads_with_error_when_no_data() {
        let t = tooltip(&snap_with(None, Some("登录态已失效")), "百炼 Token Plan");
        assert!(t.contains("登录态已失效"), "{t}");
    }

    /// 有旧数据 + 报错时，既要显示旧数据也要标注过期。
    #[test]
    fn tooltip_flags_stale_data() {
        let mut s = snap_with(Some(62.0), Some("网络超时"));
        s.last_success = Some(Utc::now());

        let t = tooltip(&s, "百炼 Token Plan");
        assert!(t.contains("62%"), "旧数据仍要显示: {t}");
        assert!(t.contains("过期"), "要标注可能过期: {t}");
    }

    /// 什么都不知道时（连 used/total 都没有），只能显示占位符，不能编造数字。
    #[test]
    fn tooltip_handles_missing_percentage() {
        let empty = UsageSnapshot {
            windows: vec![UsageWindow::new("本月")],
            unit: Some("Credits".into()),
            fetched_at: Utc::now(),
            source_label: "测试".into(),
            error: None,
            last_success: Some(Utc::now()),
        };

        let t = tooltip(&empty, "测试");
        assert!(t.contains("—"), "拿不到百分比时不应编造数字: {t}");
    }

    /// 接口没给百分比、但给了 used/total 时，应当推算出来而不是显示占位符。
    #[test]
    fn tooltip_derives_percentage_from_used_and_total() {
        let mut w = UsageWindow::new("本月");
        w.used = Some(7600.0);
        w.total = Some(20000.0);

        let s = UsageSnapshot {
            windows: vec![w],
            unit: Some("Credits".into()),
            fetched_at: Utc::now(),
            source_label: "测试".into(),
            error: None,
            last_success: Some(Utc::now()),
        };

        let t = tooltip(&s, "测试");
        assert!(t.contains("62%"), "应由 used/total 推出 62%: {t}");
        assert!(!t.contains("—"), "不该退化成占位符: {t}");
    }

    /// 单位和数字之间要有空格，别粘成 "12400.00Credits"；且保留两位小数。
    #[test]
    fn tooltip_separates_number_and_unit() {
        let t = tooltip(&snap_with(Some(62.0), None), "测试");
        assert!(t.contains("12400.00 Credits"), "应有空格且保留两位小数: {t}");
        assert!(!t.contains("12400.00Credits"), "不该粘连: {t}");
    }

    #[test]
    fn tooltip_without_windows_does_not_panic() {
        let s = UsageSnapshot::empty("测试");
        let t = tooltip(&s, "测试");
        assert!(t.contains("TokenMonitor"), "{t}");
    }
}