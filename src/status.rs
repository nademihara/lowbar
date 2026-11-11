//! The status information the compositor doesn't hand us over Wayland:
//! the clock (chrono) and the battery (sysfs).

use std::fs;

use chrono::Local;

/// Everything shown on the bar, in one comparable value — if it doesn't
/// change between ticks, we don't bother redrawing.
#[derive(PartialEq)]
pub struct Status {
    /// (name, is_active) of every workspace, in display order.
    pub workspaces: Vec<(String, bool)>,
    pub battery: Option<Battery>,
    pub clock: String,
}

impl Status {
    /// Gather a fresh snapshot of the world.
    pub fn build(workspaces: Vec<(String, bool)>) -> Status {
        Status {
            workspaces,
            battery: read_battery(),
            clock: read_clock(),
        }
    }
}

#[derive(PartialEq, Clone, Copy)]
pub struct Battery {
    pub percent: u8,
    pub charging: bool,
}

impl Battery {
    pub fn text(&self) -> String {
        let tag = if self.charging { "chg" } else { "bat" };
        format!("{tag} {:>3}%", self.percent)
    }
}

/// First battery found in /sys/class/power_supply, if any.
fn read_battery() -> Option<Battery> {
    let entries = fs::read_dir("/sys/class/power_supply").ok()?;
    for entry in entries.flatten() {
        let base = entry.path();
        let kind = match fs::read_to_string(base.join("type")) {
            Ok(k) => k,
            Err(_) => continue,
        };
        if kind.trim() != "Battery" {
            continue; // skip AC adapters and USB power sources
        }
        let percent: u8 = fs::read_to_string(base.join("capacity"))
            .ok()?
            .trim()
            .parse()
            .ok()?;
        let status = fs::read_to_string(base.join("status")).unwrap_or_default();
        return Some(Battery {
            percent,
            charging: status.trim() == "Charging",
        });
    }
    None
}

/// "Wed 11 Nov 14:23"
fn read_clock() -> String {
    Local::now().format("%a %d %b %H:%M").to_string()
}
