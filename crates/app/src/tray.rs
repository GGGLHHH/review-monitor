//! 托盘图标：关窗后程序继续在后台监控。
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

pub enum TrayAction {
    Open,
    CheckNow,
    Quit,
}

pub struct Tray {
    icon: TrayIcon,
}

const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");

fn load_icon() -> anyhow::Result<Icon> {
    let img = image::load_from_memory(ICON_PNG)?.resize(64, 64, image::imageops::FilterType::Lanczos3).into_rgba8();
    let (w, h) = img.dimensions();
    Ok(Icon::from_rgba(img.into_raw(), w, h)?)
}

impl Tray {
    pub fn new() -> anyhow::Result<Self> {
        let menu = Menu::new();
        menu.append_items(&[
            &MenuItem::with_id("open", "打开差评监控", true, None),
            &MenuItem::with_id("check", "立即检查", true, None),
            &PredefinedMenuItem::separator(),
            &MenuItem::with_id("quit", "退出", true, None),
        ])?;
        let icon = TrayIconBuilder::new()
            .with_icon(load_icon()?)
            .with_tooltip("差评监控")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()?;
        Ok(Self { icon })
    }

    pub fn set_tooltip(&self, s: &str) {
        let _ = self.icon.set_tooltip(Some(s));
    }

    /// 取出所有待处理的托盘事件
    pub fn poll() -> Vec<TrayAction> {
        let mut v = Vec::new();
        while let Ok(e) = MenuEvent::receiver().try_recv() {
            match e.id.as_ref() {
                "open" => v.push(TrayAction::Open),
                "check" => v.push(TrayAction::CheckNow),
                "quit" => v.push(TrayAction::Quit),
                _ => {}
            }
        }
        while let Ok(e) = TrayIconEvent::receiver().try_recv() {
            match e {
                TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
                | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } => v.push(TrayAction::Open),
                _ => {}
            }
        }
        v
    }
}
