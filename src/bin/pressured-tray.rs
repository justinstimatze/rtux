// pressured-tray — an ambient top-bar memory-pressure light, as a
// StatusNotifierItem. Works with no logout (unlike a GNOME Shell extension):
// it shows up live via the ubuntu-appindicators support. Reads kernel PSI
// directly every 2s, recolours a dot green/amber/red, and opens the HUD on
// click. Build with:  cargo build --release --features tray

use std::fs;
use std::process::Command;
use std::thread;
use std::time::Duration;

use ksni::blocking::TrayMethods;
use ksni::{Icon, ToolTip, Tray};

fn classify() -> &'static str {
    let text = fs::read_to_string("/proc/pressure/memory").unwrap_or_default();
    let (mut some, mut full) = (0.0f64, 0.0f64);
    for line in text.lines() {
        if let Some(idx) = line.find("avg10=") {
            let rest = &line[idx + 6..];
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            let v: f64 = rest[..end].parse().unwrap_or(0.0);
            if line.starts_with("some") {
                some = v;
            } else if line.starts_with("full") {
                full = v;
            }
        }
    }
    if some > 25.0 || full > 10.0 {
        "critical"
    } else if some > 5.0 {
        "elevated"
    } else {
        "normal"
    }
}

fn color(level: &str) -> (u8, u8, u8) {
    match level {
        "critical" => (0xe2, 0x50, 0x4a),
        "elevated" => (0xe0, 0xa5, 0x3b),
        _ => (0x6c, 0xc0, 0x4a),
    }
}

/// A filled circle in the level colour, as an ARGB32 pixmap.
fn circle_icon(level: &str) -> Icon {
    let size: i32 = 22;
    let (r, g, b) = color(level);
    let mut data = vec![0u8; (size * size * 4) as usize];
    let c = (size as f32 - 1.0) / 2.0;
    let rad = size as f32 / 2.0 - 2.0;
    for y in 0..size {
        for x in 0..size {
            let (dx, dy) = (x as f32 - c, y as f32 - c);
            let dist = (dx * dx + dy * dy).sqrt();
            let i = ((y * size + x) * 4) as usize;
            // soft 1px edge
            let alpha = if dist <= rad {
                255.0
            } else if dist <= rad + 1.0 {
                255.0 * (rad + 1.0 - dist)
            } else {
                0.0
            };
            data[i] = alpha as u8; // A
            data[i + 1] = r; // R
            data[i + 2] = g; // G
            data[i + 3] = b; // B
        }
    }
    Icon { width: size, height: size, data }
}

/// Open/raise the HUD. Wayland only grants focus to a *fresh* client, so we
/// SIGKILL any running HUD and spawn a new process (which focuses on map). Kept
/// in sync with `mitigate::SUMMON_HUD` / `setup-hotkey.sh` — separate bins can't
/// share a const without a lib crate, so the literal is mirrored here.
fn open_hud() {
    let _ = Command::new("sh")
        .args([
            "-c",
            "pkill -KILL -x pressured-hud; for i in $(seq 50); do pgrep -x pressured-hud >/dev/null || break; sleep 0.02; done; pressured-hud",
        ])
        .spawn();
}

struct RtuxTray {
    level: String,
}

impl Tray for RtuxTray {
    fn id(&self) -> String {
        "rtux".into()
    }
    fn title(&self) -> String {
        "rtux".into()
    }
    fn icon_pixmap(&self) -> Vec<Icon> {
        vec![circle_icon(&self.level)]
    }
    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: format!("Memory pressure: {}", self.level),
            description: "Click to open the rtux control window".into(),
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
        }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        open_hud();
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem {
                label: "Open rtux window".into(),
                activate: Box::new(|_: &mut Self| open_hud()),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit indicator".into(),
                activate: Box::new(|_: &mut Self| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

fn main() {
    // Auto-reap the HUD-launch children we spawn so they never pile up as zombies.
    unsafe {
        let _ = nix::sys::signal::signal(
            nix::sys::signal::Signal::SIGCHLD,
            nix::sys::signal::SigHandler::SigIgn,
        );
    }

    // Register with the StatusNotifierWatcher, retrying until it appears — at
    // login the tray may start before the shell's appindicator support is up,
    // and the watcher can come and go if the shell reloads. Never panic.
    let handle = loop {
        let tray = RtuxTray {
            level: classify().to_string(),
        };
        match tray.spawn() {
            Ok(h) => break h,
            Err(e) => {
                eprintln!("tray: StatusNotifierWatcher not ready ({e}); retrying in 3s");
                thread::sleep(Duration::from_secs(3));
            }
        }
    };

    loop {
        thread::sleep(Duration::from_secs(2));
        let level = classify().to_string();
        let _ = handle.update(move |t: &mut RtuxTray| t.level = level.clone());
    }
}
