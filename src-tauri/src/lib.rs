//! TokenMonitor —— 千问/百炼 Token Plan 用量监控桌面小工具。
//!
//! 整体结构：
//! - [`config`] 配置模型与持久化（敏感字段剥离）
//! - [`secret`] 敏感凭据 → Windows 凭据管理器
//! - [`usage`] 取数与归一化（三条数据源）
//! - [`icon`] 托盘图标的位图绘制
//! - [`tray`] 托盘菜单与图标更新
//! - [`notify`] 额度不足提醒的状态机
//!
//! 前端是 `../src` 下的静态 HTML/JS，**不用打包器**，靠 `withGlobalTauri` 拿到
//! `window.__TAURI__`。

pub mod config;
pub mod icon;
pub mod notify;
pub mod secret;
pub mod tray;
pub mod usage;

use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, Runtime, State};

use config::{AppConfig, SourceMode};
use notify::AlertState;
use usage::provider;
use usage::UsageSnapshot;

/// 快照更新时广播给前端的事件名
pub const EVENT_SNAPSHOT: &str = "snapshot";

/// 全局状态。
pub struct AppState {
    pub config: Mutex<AppConfig>,
    pub snapshot: Mutex<UsageSnapshot>,
    /// 最近一次实际生效的数据源（Auto 模式下会变），用于界面显示
    pub active_source: Mutex<Option<SourceMode>>,
    pub http: reqwest::Client,
    pub alert: Mutex<AlertState>,
}

impl AppState {
    fn new(cfg: AppConfig) -> Self {
        Self {
            config: Mutex::new(cfg),
            snapshot: Mutex::new(UsageSnapshot::empty("尚未取数")),
            active_source: Mutex::new(None),
            // 客户端构造失败时退到一个默认实例，不让程序起不来
            http: provider::build_http_client().unwrap_or_default(),
            alert: Mutex::new(AlertState::default()),
        }
    }
}

// ───────────────────────── 取数与广播 ─────────────────────────

/// 刷新一次，并把结果同步到状态、托盘和前端。
///
/// 托盘菜单的「立即刷新」、设置页的刷新按钮、后台定时轮询都走这一个入口，
/// 保证行为一致。
pub async fn refresh_and_broadcast<R: Runtime>(app: AppHandle<R>) -> UsageSnapshot {
    let (cfg, http) = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().map(|c| c.clone()).unwrap_or_default();
        let http = state.http.clone();
        (cfg, http)
    };

    let creds = secret::load_all();
    let result = provider::fetch(&http, &cfg, &creds).await;

    let state = app.state::<AppState>();

    let snap = match result {
        Ok((snap, mode)) => {
            if let Ok(mut m) = state.active_source.lock() {
                *m = Some(mode);
            }
            // 额度告警
            let pct = snap.most_constrained().and_then(|w| w.effective_remaining_pct());
            let should = state
                .alert
                .lock()
                .map(|mut a| a.should_alert(pct, cfg.clamped_threshold()))
                .unwrap_or(false);
            if should {
                send_low_quota_notification(&app, pct.unwrap_or(0.0), &snap);
            }
            snap
        }
        Err(e) => {
            // 关键：失败时保留上一次的成功数据，只挂上错误
            let prev = state
                .snapshot
                .lock()
                .map(|s| s.clone())
                .unwrap_or_else(|_| UsageSnapshot::empty("取数失败"));
            prev.with_error(e.user_message())
        }
    };

    if let Ok(mut s) = state.snapshot.lock() {
        *s = snap.clone();
    }

    tray::update(&app, &snap, &cfg);
    let _ = app.emit(EVENT_SNAPSHOT, &snap);

    snap
}

fn send_low_quota_notification<R: Runtime>(app: &AppHandle<R>, pct: f64, snap: &UsageSnapshot) {
    use tauri_plugin_notification::NotificationExt;

    let label = snap
        .windows
        .first()
        .map(|w| w.label.clone())
        .unwrap_or_else(|| "额度".into());

    let body = match snap.windows.first().and_then(|w| w.remaining_amount()) {
        Some(rem) => format!(
            "{label}剩余 {rem:.0}{}（{pct:.0}%），请留意用量",
            snap.unit.as_deref().unwrap_or("")
        ),
        None => format!("{label}剩余仅 {pct:.0}%，请留意用量"),
    };

    let _ = app
        .notification()
        .builder()
        .title("Token Plan 额度不足")
        .body(body)
        .show();
}

/// 启动后台定时轮询。
fn spawn_scheduler<R: Runtime>(app: AppHandle<R>) {
    tauri::async_runtime::spawn(async move {
        // 启动后先拉一次，但稍等一下，让窗口先画出来
        tokio::time::sleep(Duration::from_millis(800)).await;
        refresh_and_broadcast(app.clone()).await;

        loop {
            let secs = {
                let state = app.state::<AppState>();
                state
                    .config
                    .lock()
                    .map(|c| c.clamped_interval())
                    .unwrap_or(60)
            };
            tokio::time::sleep(Duration::from_secs(secs)).await;
            refresh_and_broadcast(app.clone()).await;
        }
    });
}

// ───────────────────────── 窗口操作 ─────────────────────────

/// 切换悬浮卡片的显隐。
pub fn toggle_card<R: Runtime>(app: &AppHandle<R>) {
    let Some(win) = app.get_webview_window("card") else {
        return;
    };
    match win.is_visible() {
        Ok(true) => {
            let _ = win.hide();
        }
        _ => {
            let _ = win.show();
            let _ = win.set_always_on_top(true);
        }
    }
}

/// 给设置窗口挂上「点 X 只隐藏，不销毁」的行为。
///
/// Tauri 默认点关闭是**销毁**窗口；一旦销毁，`get_webview_window("settings")`
/// 就返回 `None`，托盘菜单再也打不开设置。这是必须拦下来的。
fn attach_settings_close_handler<R: Runtime>(app: &AppHandle<R>, win: &tauri::WebviewWindow<R>) {
    let handle = app.clone();
    win.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            if let Some(w) = handle.get_webview_window("settings") {
                let _ = w.hide();
            }
        }
    });
}

/// 把保存的窗口位置夹进显示器可视范围。
///
/// 返回 `None` 表示这个位置**根本不在任何显示器上**（换过显示器、改过分辨率、
/// 拔掉过外接屏之后都可能出现）。此时若照原样恢复，卡片会永远看不见，
/// 而且没有任何界面操作能救回来——托盘只会显示/隐藏，位置仍然是坏的。
///
/// 位置在屏上但**放不下整个窗口**时（例如卡片被加宽后右边超出屏幕），
/// 往回收，否则边缘的按钮会被裁掉。
fn clamp_position_to_monitor<R: Runtime>(
    app: &AppHandle<R>,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
) -> Option<(i32, i32)> {
    let monitors = match app.available_monitors() {
        Ok(m) => m,
        // 查不到显示器信息时别拦，宁可照原样恢复
        Err(_) => return Some((x, y)),
    };

    let m = monitors.iter().find(|m| {
        let p = m.position();
        let s = m.size();
        x >= p.x && x < p.x + s.width as i32 && y >= p.y && y < p.y + s.height as i32
    })?;

    let p = m.position();
    let s = m.size();
    // 上限要防负数：窗口比屏幕还宽时 max 会小于 min，clamp 会 panic
    let max_x = (p.x + s.width as i32 - w as i32).max(p.x);
    let max_y = (p.y + s.height as i32 - h as i32).max(p.y);

    Some((x.clamp(p.x, max_x), y.clamp(p.y, max_y)))
}

/// 打开设置窗口（已开着就聚焦）。
pub fn show_settings<R: Runtime>(app: &AppHandle<R>) {
    if let Some(win) = app.get_webview_window("settings") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
        return;
    }

    // 兜底：窗口若因某种原因被销毁了，就地重建一个。
    // 不做这层的话，"设置再也打不开"会变成一个只能重启程序才能恢复的故障。
    match tauri::WebviewWindowBuilder::new(
        app,
        "settings",
        tauri::WebviewUrl::App("settings.html".into()),
    )
    .title("TokenMonitor 设置")
    .inner_size(720.0, 760.0)
    .center()
    .build()
    {
        Ok(win) => {
            attach_settings_close_handler(app, &win);
            let _ = win.set_focus();
        }
        Err(e) => eprintln!("重建设置窗口失败: {e}"),
    }
}

// ───────────────────────── 命令 ─────────────────────────

#[tauri::command]
fn get_snapshot(state: State<'_, AppState>) -> UsageSnapshot {
    state
        .snapshot
        .lock()
        .map(|s| s.clone())
        .unwrap_or_else(|_| UsageSnapshot::empty("状态不可用"))
}

#[tauri::command]
async fn refresh_now(app: AppHandle) -> Result<UsageSnapshot, String> {
    Ok(refresh_and_broadcast(app).await)
}

#[tauri::command]
fn get_config(state: State<'_, AppState>) -> AppConfig {
    state.config.lock().map(|c| c.clone()).unwrap_or_default()
}

#[tauri::command]
fn get_active_source(state: State<'_, AppState>) -> Option<SourceMode> {
    state.active_source.lock().ok().and_then(|m| *m)
}

#[tauri::command]
async fn save_config(app: AppHandle, cfg: AppConfig) -> Result<(), String> {
    cfg.save().map_err(|e| format!("保存失败: {e}"))?;

    // 把 guard 绑到具名变量上再离开作用域。
    // 不能写成 `if let Ok(mut c) = state.config.lock()`——if let 会把 scrutinee
    // 的临时量延长到整个块尾，而 MutexGuard 借用了 State，State 又借用了
    // app.state() 的返回值，这条链在 async fn 里活不过 await。
    {
        let state = app.state::<AppState>();
        let mut c = state.config.lock().unwrap_or_else(|e| e.into_inner());
        *c = cfg.clone();
    }
    // 阈值可能变了，让提醒状态机重置，新阈值立刻生效
    {
        let state = app.state::<AppState>();
        let mut a = state.alert.lock().unwrap_or_else(|e| e.into_inner());
        a.reset();
    }

    // 立即按新配置刷一次，界面能马上看到效果
    refresh_and_broadcast(app).await;
    Ok(())
}

/// 哪些凭据已经配置好了——只回布尔值，**不回传凭据本身**。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretStatus {
    pub cookie: bool,
    pub ak_secret: bool,
    pub generic: bool,
    /// 系统凭据管理器是否可用；不可用时要提示用户，而不是等保存时才报错
    pub store_available: bool,
}

#[tauri::command]
fn get_secret_status() -> SecretStatus {
    SecretStatus {
        cookie: secret::get(secret::KEY_COOKIE).is_some(),
        ak_secret: secret::get(secret::KEY_AK_SECRET).is_some(),
        generic: secret::get(secret::KEY_GENERIC).is_some(),
        store_available: secret::store_available(),
    }
}

#[tauri::command]
fn set_secret(key: String, value: String) -> Result<(), String> {
    secret::set(&key, &value).map_err(|e| e.to_string())
}

#[tauri::command]
fn clear_secret(key: String) -> Result<(), String> {
    secret::delete(&key).map_err(|e| e.to_string())
}

#[tauri::command]
fn set_widget_locked(app: AppHandle, locked: bool) -> Result<(), String> {
    let state = app.state::<AppState>();
    if let Ok(mut c) = state.config.lock() {
        c.widget.locked = locked;
    }
    // 位置锁定后不再保存新位置，所以这里持久化
    let cfg = state
        .config
        .lock()
        .map(|c| c.clone())
        .unwrap_or_default();
    cfg.save().map_err(|e| e.to_string())
}

#[tauri::command]
fn hide_card(app: AppHandle) {
    if let Some(win) = app.get_webview_window("card") {
        let _ = win.hide();
    }
}

#[tauri::command]
fn open_settings(app: AppHandle) {
    show_settings(&app);
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

/// 按前端量出来的内容高度调整卡片高度。
///
/// 前端给的是 **CSS 像素**，而窗口尺寸要 **物理像素**，所以要乘缩放系数——
/// 高 DPI 屏上漏掉这一步会让窗口只有应有高度的一半。
///
/// 宽度保持不变（由配置决定），只调高度。
#[tauri::command]
fn resize_card(app: AppHandle, height: f64) -> Result<(), String> {
    let Some(win) = app.get_webview_window("card") else {
        return Ok(());
    };

    // 夹一个范围，避免前端算出异常值时把窗口变成一条线或撑满屏幕
    let css_h = height.clamp(110.0, 640.0);
    let scale = win.scale_factor().map_err(|e| e.to_string())?;
    let phys_h = (css_h * scale).round() as u32;

    let cur = win.inner_size().map_err(|e| e.to_string())?;
    if cur.height == phys_h {
        return Ok(());
    }

    win.set_size(tauri::PhysicalSize::new(cur.width, phys_h))
        .map_err(|e| e.to_string())
}

/// 开关开机自启。
///
/// 插件的 JS 包不在 `withGlobalTauri` 的暴露范围内，而为它引入 npm 打包链不值得，
/// 所以直接用 Rust 命令暴露。
///
/// 注意：该插件对**需要管理员权限**的程序在 Windows 上会失效；本工具不需提权，
/// 正常工作。
#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;

    let mgr = app.autolaunch();
    let r = if enabled { mgr.enable() } else { mgr.disable() };
    r.map_err(|e| format!("设置开机自启失败: {e}"))
}

/// 读开机自启的真实状态（以系统为准，不信配置文件）。
#[tauri::command]
fn get_autostart(app: AppHandle) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt;

    app.autolaunch()
        .is_enabled()
        .map_err(|e| format!("读取开机自启状态失败: {e}"))
}

// ───────────────────────── 入口 ─────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let cfg = AppConfig::load_or_default();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(AppState::new(cfg))
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            refresh_now,
            get_config,
            get_active_source,
            save_config,
            get_secret_status,
            set_secret,
            clear_secret,
            set_widget_locked,
            hide_card,
            open_settings,
            quit_app,
            set_autostart,
            get_autostart,
            resize_card,
        ])
        .setup(|app| {
            // 恢复上次的窗口位置
            if let Some(win) = app.get_webview_window("card") {
                // 用 .ok() 转成 Option：LockResult 的 Err 分支里含着 MutexGuard，
                // 会让 app.state() 的临时值活到块尾，借用检查过不去
                let cfg = app
                    .state::<AppState>()
                    .config
                    .lock()
                    .ok()
                    .map(|c| c.clone());
                if let Some(cfg) = cfg {
                    if let (Some(x), Some(y)) = (cfg.widget.x, cfg.widget.y) {
                        let size = win
                            .outer_size()
                            .unwrap_or(tauri::PhysicalSize::new(450, 200));
                        match clamp_position_to_monitor(app.handle(), x, y, size.width, size.height)
                        {
                            Some((cx, cy)) => {
                                if (cx, cy) != (x, y) {
                                    eprintln!("卡片位置 ({x}, {y}) 越界，收到 ({cx}, {cy})");
                                }
                                let _ = win.set_position(tauri::PhysicalPosition::new(cx, cy));
                            }
                            None => {
                                // 位置不在任何屏幕上，用回默认位置
                                eprintln!(
                                    "保存的卡片位置 ({x}, {y}) 不在任何显示器上，回退到默认位置"
                                );
                            }
                        }
                    }
                    if !cfg.widget.visible {
                        let _ = win.hide();
                    }
                }

                // 拖动后记住位置，下次原位恢复
                let handle = app.handle().clone();
                win.on_window_event(move |event| {
                    if let tauri::WindowEvent::Moved(pos) = event {
                        let state = handle.state::<AppState>();
                        let Ok(mut cfg) = state.config.lock() else {
                            return;
                        };
                        if cfg.widget.locked {
                            return;
                        }
                        cfg.widget.x = Some(pos.x);
                        cfg.widget.y = Some(pos.y);
                        let snapshot = cfg.clone();
                        drop(cfg);
                        // 位置变化很频繁，失败也无所谓，不必打扰用户
                        let _ = snapshot.save();
                    }
                });
            }

            // 设置窗口点 X 只隐藏，不销毁（否则托盘再也打不开它）
            if let Some(win) = app.get_webview_window("settings") {
                attach_settings_close_handler(app.handle(), &win);
            }

            tray::setup(app.handle())?;
            spawn_scheduler(app.handle().clone());

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("TokenMonitor 启动失败");
}