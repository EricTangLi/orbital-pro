//! 托盘与生命周期（`docs/02 §3.4/§3.5` · ADR-016）
//!
//! 规则：
//! - **关闭主窗 = 隐藏**，进程继续跑（托盘常驻）；
//! - **左键**托盘 → 显示/隐藏主窗；
//! - **右键** → 自绘 WebView 面板（320×300 逻辑像素，不是原生菜单；ADR-016 已否原生菜单）；
//! - 真退出只从托盘菜单走，且由 T-M1-06 的 `RunEvent::Exit` 清理三进程。

use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};

/// ★ 必须持有：TrayIcon 是引用计数类型，最后一个实例 drop 时**图标会被移除**
/// （Tauri 2 官方文档原文："The icon is removed when the last instance is dropped"）。
/// 不持有它 = 图标建完立刻消失（这正是第一次"看不到图标"的根因）。
pub struct TrayHandle(#[allow(dead_code)] pub tauri::tray::TrayIcon);

pub const MAIN: &str = "main";

/// 自己维护"主窗是否显示"：Tauri 的 is_visible() 在 config 初始 isible:false 后会失真
/// （实测：窗口明明在屏幕上，is_visible() 返回 Ok(false)，导致 toggle 永远走 show 分支）。
static MAIN_VISIBLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn mark_visible(v: bool) {
    MAIN_VISIBLE.store(v, std::sync::atomic::Ordering::SeqCst);
}
pub const PANEL: &str = "tray-panel";

/// T-M8-01：面板里的「打开主窗口」按钮。
#[tauri::command]
pub fn show_main_window(app: AppHandle) {
    show_main(&app);
}

/// T-M8-01：面板里的「退出」按钮（走正常退出流程，会清理子进程）。
#[tauri::command]
pub fn quit_app(app: AppHandle) {
    app.exit(0);
}

/// T-M10-38：面板的**显式关闭**。
///
/// 此前只有"失焦自动隐藏"（而且首版连这个都没实现），面板上没有 × 按钮——
/// 用户报"弹出对话框无法收回"。现在两个途径都能关：失焦 + 按钮/Esc。
#[tauri::command]
pub fn hide_panel(app: AppHandle) {
    if let Some(w) = app.get_webview_window(PANEL) {
        let _ = w.hide();
        tracing::info!("tray panel hidden (× / Esc)");
    }
}

/// 建托盘。失败只记日志，不阻断应用启动。
pub fn build(app: &AppHandle) -> tauri::Result<()> {
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))?;

    let tray = TrayIconBuilder::with_id("orbital-tray")
        .tooltip("Orbital Pro · 记忆中枢")
        .icon(icon)
        .on_tray_icon_event(|tray, ev| {
            // T-M10-46：`Move` 在鼠标划过托盘时每毫秒级触发一次。之前按 INFO 全量打印，
            // 实测一天日志近 200KB 全是 `Move { position: … }`，把真正有用的
            // `panel positioned` / `toggle` 全淹了。⇒ 只有 Click 记 INFO，其余降 trace。
            if !matches!(ev, TrayIconEvent::Click { .. }) {
                tracing::trace!(event = ?ev, "tray icon move/other event");
                return;
            }
            tracing::info!(event = ?ev, "tray icon clicked");
            match ev {
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } => toggle_main(tray.app_handle()),
                // ADR-016：右键弹**自绘 WebView 面板**，不是原生菜单
                // T-M10-45：把 tray 事件自带的 `rect`（图标精确位置+尺寸）传下去——
                // 这是 Tauri 2 的官方能力，不用猜鼠标坐标或写死屏幕角落。
                TrayIconEvent::Click {
                    button: MouseButton::Right,
                    button_state: MouseButtonState::Up,
                    rect,
                    ..
                } => show_panel(tray.app_handle(), rect),
                _ => {}
            }
        })
        .build(app)?;
    tracing::info!(id = ?tray.id(), "tray icon built");
    // ★ 交给 Tauri 状态管理持有；返回类型决定它不能被丢弃
    let managed = app.manage(TrayHandle(tray));
    tracing::info!(managed, "tray handle managed (false=already existed)");
    Ok(())
}

pub fn show_main(app: &AppHandle) {
    mark_visible(true);
    if let Some(w) = app.get_webview_window(MAIN) {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

pub fn toggle_main(app: &AppHandle) {
    let visible = MAIN_VISIBLE.load(std::sync::atomic::Ordering::SeqCst);
    tracing::info!(visible, "toggle: using tracked state");
    match app.get_webview_window(MAIN) {
        None => tracing::warn!("toggle: main window not found"),
        Some(w) => {
            if visible {
                let r = w.hide();
                mark_visible(false);
                tracing::info!(?r, "toggle: hidden");
            } else {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
                mark_visible(true);
                tracing::info!("toggle: shown");
            }
        }
    }
}

/// T-M10-48：失焦隐藏的"布防"标志。
///
/// 真机实测（2026-11-16 19:17，从**溢出区**右键）：`panel positioned` 日志已经出现，
/// 但窗口列表里没有面板 —— 面板 show 之后立刻收到 `Focused(false)` 又被 hide 了。
/// 原因：溢出面板收起的过渡动画期间会把焦点还给主窗，此时我们刚显示的面板必然失焦。
/// ⇒ 每次 show 后先"撤防"，等 400ms 过渡结束再"布防"，避免瞬时失焦误关。
static PANEL_ARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn disarm_panel_focus_hide() {
    PANEL_ARMED.store(false, std::sync::atomic::Ordering::SeqCst);
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(400));
        PANEL_ARMED.store(true, std::sync::atomic::Ordering::SeqCst);
        tracing::info!("tray panel focus-hide armed");
    });
}

/// 自绘面板：无边框、置顶、不进任务栏，**失焦自动隐藏**。
///
/// T-M10-38（2026-11-16 用户报"弹出对话框无法收回"）：
/// 首版注释写了"失焦自动隐藏"，**但代码里没有任何失焦处理** ——
/// 面板弹出后点别处、点主窗、切到别的应用，它都**留在屏幕上**。
/// 修法：`on_window_event` 监听 `Focused(false)` → `hide()`。
pub fn show_panel(app: &AppHandle, icon: tauri::Rect) {
    if let Some(w) = app.get_webview_window(PANEL) {
        // T-M10-43（2026-11-16 用户实测）：**再点一次 = 二次弹出**。
        // 原因：这里对已存在的窗口无条件 `show()`，没有判断它是否已经可见。
        // 改成 toggle：可见就收起，不可见才显示。
        let visible = w.is_visible().unwrap_or(false);
        if visible {
            let _ = w.hide();
        } else {
            position_panel(&w, app, icon);
            let _ = w.show();
            let _ = w.set_focus();
            disarm_panel_focus_hide();
            tracing::info!(
                focused = w.is_focused().unwrap_or(false),
                "tray panel shown (reused)"
            );
        }
        return;
    }
    let built = tauri::WebviewWindowBuilder::new(
        app,
        PANEL,
        tauri::WebviewUrl::App("index.html?panel=1".into()),
    )
    .title("Orbital")
    .inner_size(320.0, 300.0)
    .resizable(false)
    .decorations(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .visible(false)
    // T-M10-48：托盘面板必须能抢到前台，否则收不到 Focused 事件 ⇒ 点别处也收不回
    .focused(true)
    .build();

    match built {
        Ok(w) => {
            // ★ 失焦即隐藏：这是"点别处能收回"的唯一实现点。
            // T-M10-48：监听器只在创建时挂一次，是否生效由 PANEL_ARMED 决定
            //（每次 show 都会先撤防 400ms，避开创建/溢出面板收起时的瞬时失焦）。
            let win = w.clone();
            w.on_window_event(move |event| {
                if matches!(event, tauri::WindowEvent::Focused(false))
                    && PANEL_ARMED.load(std::sync::atomic::Ordering::SeqCst)
                    // T-M10-48：只对**仍可见**的窗口 hide 并记日志。
                    // 否则 hide 自身又产生一次 Focused(false)，实测一条链路刷 4 条重复日志。
                    && win.is_visible().unwrap_or(false)
                {
                    let _ = win.hide();
                    tracing::info!("tray panel hidden (lost focus)");
                }
            });
            position_panel(&w, app, icon);
            let _ = w.show();
            let _ = w.set_focus();
            disarm_panel_focus_hide();
            tracing::info!(
                focused = w.is_focused().unwrap_or(false),
                "tray panel shown (built)"
            );
        }
        Err(e) => tracing::error!(%e, "build tray panel failed"),
    }
}

/// T-M10-45/46：按**托盘图标矩形 + 显示器工作区**定位（Tauri 2 官方能力，不猜坐标）。
///
/// 调研依据（读 tauri-2.12.5 / tray-icon 源码确认）：
///   - `TrayIconEvent::Click` 自带 `rect: Rect { position, size }` —— 图标的**精确**位置和尺寸
///   - `AppHandle::monitor_from_point(x, y)` —— 图标在哪个显示器
///   - `Monitor::work_area()` —— **排除任务栏**的可用区域
///
/// 规则（Windows 托盘菜单通例，与原生 Shell 菜单一致）：
///   1. 默认贴图标**上方**，右边缘与图标对齐
///   2. 上方放不下（越出工作区顶部）→ 放到图标**下方**
///   3. 最后 clamp 到 `work_area` —— 任务栏不在工作区内，**底部不会再被遮挡**
///
/// T-M10-46（2026-11-16 真机实测）：**所有单位必须统一为物理像素**。
/// 旧实现把逻辑尺寸 `(320,520)` 与物理的 `work_area` 混算，在本机 **175% 缩放**下：
///   - 窗口真实物理尺寸是 560×735，却被当成 320×520 参与计算
///   - 于是右边缘比预期多伸出 240px，顶部 y 又按 520 算 ⇒ 底部溢出屏幕 131px
///   ⇒ 用户看到的"底部被遮挡 / 显示不完"，根因就在这里，不是留白不够。
fn position_panel(w: &tauri::WebviewWindow, app: &AppHandle, icon: tauri::Rect) {
    // 设计稿尺寸（逻辑像素）；实际落地尺寸 = 逻辑 × 缩放，统一换算成物理像素后再参与定位
    const WANT_W: f64 = 320.0;
    const WANT_H: f64 = 300.0;
    let scale = w.scale_factor().unwrap_or(1.0);
    let pw = (WANT_W * scale).round();
    let ph = (WANT_H * scale).round();

    // 显式按物理像素设尺寸：
    // window-state 已把 tray-panel 加入 denylist（不再恢复旧尺寸），这里再设一次双保险，
    // 防止旧状态文件残留或平台自行调整（实测旧状态里是 560×735）。
    let _ = w.set_size(tauri::PhysicalSize::new(pw as u32, ph as u32));

    // tauri::Rect 的 position/size 是**枚举**（Physical | Logical）—— Logical 必须乘缩放
    let (icon_x, icon_y) = match icon.position {
        tauri::Position::Physical(p) => (p.x as f64, p.y as f64),
        tauri::Position::Logical(p) => (p.x * scale, p.y * scale),
    };
    let (icon_w, icon_h) = match icon.size {
        tauri::Size::Physical(s) => (s.width as f64, s.height as f64),
        tauri::Size::Logical(s) => (s.width * scale, s.height * scale),
    };

    let mon = match app.monitor_from_point(icon_x, icon_y) {
        Ok(Some(m)) => m,
        _ => {
            tracing::warn!("monitor_from_point 失败，面板用默认位置");
            return;
        }
    };
    let wa = mon.work_area();
    let wa_x = wa.position.x as f64;
    let wa_y = wa.position.y as f64;
    let wa_w = wa.size.width as f64;
    let wa_h = wa.size.height as f64;

    // ① 默认：图标上方、右对齐
    let mut x = icon_x + icon_w - pw;
    let mut y = icon_y - ph;
    // ② 上方不够 → 放图标下方
    if y < wa_y {
        y = icon_y + icon_h;
    }
    // ③ 夹到工作区（等高/等宽时 clamp 的 min>max 会 panic，先取 max）
    x = x.clamp(wa_x, (wa_x + wa_w - pw).max(wa_x));
    y = y.clamp(wa_y, (wa_y + wa_h - ph).max(wa_y));

    let _ = w.set_position(tauri::PhysicalPosition::new(x as i32, y as i32));
    tracing::info!(
        x,
        y,
        pw,
        ph,
        scale,
        icon_x,
        icon_y,
        wa_x,
        wa_y,
        wa_w,
        wa_h,
        "panel positioned (physical units)"
    );
}

/// T-M10-46：`--open-panel` 调试路径的锚点。
///
/// 此前传 `Rect::default()`（原点、零尺寸）⇒ 面板永远固定在屏幕左上角，
/// 与右键路径不一致，也没法用它验证"贴托盘"的定位。改成伪图标 = 主显示器工作区右下角，
/// 走**同一条** `position_panel` 逻辑（图标 56×84，与任务栏实际按钮尺寸一致）。
pub fn debug_panel_anchor(app: &AppHandle) -> tauri::Rect {
    if let Ok(Some(m)) = app.primary_monitor() {
        let wa = m.work_area();
        let icon_w = 56i32;
        let icon_h = 84i32;
        let x = wa.position.x + wa.size.width as i32 - icon_w;
        let y = wa.position.y + wa.size.height as i32 - icon_h;
        return tauri::Rect {
            position: tauri::Position::Physical(tauri::PhysicalPosition::new(x, y)),
            size: tauri::Size::Physical(tauri::PhysicalSize::new(
                icon_w as u32,
                icon_h as u32,
            )),
        };
    }
    tauri::Rect::default()
}
