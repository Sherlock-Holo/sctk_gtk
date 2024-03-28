use std::process::Command;

use tracing::warn;

use crate::pointer::ButtonKind;

/// Query system configuration for buttons layout.
/// Should be updated to use standard xdg-desktop-portal specs once available
/// https://github.com/flatpak/xdg-desktop-portal/pull/996
fn get_button_layout_config() -> Option<(String, String)> {
    let config_string = Command::new("dbus-send")
        .arg("--reply-timeout=100")
        .arg("--print-reply=literal")
        .arg("--dest=org.freedesktop.portal.Desktop")
        .arg("/org/freedesktop/portal/desktop")
        .arg("org.freedesktop.portal.Settings.Read")
        .arg("string:org.gnome.desktop.wm.preferences")
        .arg("string:button-layout")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())?;

    let sides_split: Vec<_> = config_string
        // Taking last word
        .rsplit(' ')
        .next()?
        // Split by left/right side
        .split(':')
        // Only two sides
        .take(2)
        .collect();

    match sides_split.as_slice() {
        [left, right] => Some((left.to_string(), right.to_string())),
        _ => None,
    }
}

/// when bool is true, means buttons should be at the end of frame, otherwise at the start
pub fn get_button_layout() -> (bool, Vec<ButtonKind>) {
    match get_button_layout_config() {
        None => {
            warn!("get button layout config failed, use default config");

            (
                true,
                vec![
                    ButtonKind::Minimize,
                    ButtonKind::Maximize,
                    ButtonKind::Close,
                ],
            )
        }

        Some((left, right)) => {
            println!("{left} {right}");

            let buttons = collect_buttons(&left);
            if !buttons.is_empty() {
                return (false, buttons);
            }

            let buttons = collect_buttons(&right);
            if !buttons.is_empty() {
                return (true, buttons);
            }

            warn!("unknown button layout config, use default config");

            (
                true,
                vec![
                    ButtonKind::Minimize,
                    ButtonKind::Maximize,
                    ButtonKind::Close,
                ],
            )
        }
    }
}

fn collect_buttons(config: &str) -> Vec<ButtonKind> {
    let mut buttons = config
        .split(',')
        .take(3)
        .filter_map(|kind| match kind {
            "close" => Some(ButtonKind::Close),
            "maximize" => Some(ButtonKind::Maximize),
            "minimize" => Some(ButtonKind::Minimize),
            other => {
                warn!(other, "unsupported button");

                None
            }
        })
        .collect::<Vec<_>>();

    // make sure the right order in gtk head bar
    buttons.reverse();

    buttons
}
