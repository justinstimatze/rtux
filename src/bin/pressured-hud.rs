// pressured-hud — a hotkey-summoned GTK4 control window for the pressured daemon.
//
// Thin client: all privilege lives in the daemon, reached over /run/pressured.sock.
// This binary renders live state and sends list/act requests. Build with:
//   cargo build --release --features hud
//
// Design: an htop-dense, theme-blending control panel. Rows are compact,
// selectable status lines with a per-app memory meter; a fixed bottom action bar
// operates on the selected app so buttons never move or truncate. Rows update in
// place and only re-sort while the pointer is outside the window, so nothing
// jumps under the cursor. Colour is confined to the data layer (good/warn/crit
// meters); everything else defers to the system (Yaru/Adwaita) theme.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::time::Duration;

use gtk4 as gtk;
use libadwaita as adw;
use gtk::glib;
use gtk::pango;
// adw's prelude re-exports gtk's, so this one glob covers both toolkits.
use adw::prelude::*;

const SOCKET_PATH: &str = "/run/pressured.sock";
const APP_ID: &str = "dev.pressured.Hud";

/// Bumped on every (re)build of the window. The live-refresh timer captures the
/// generation it was born under and stops itself once a newer window exists, so
/// recreate-on-summon never leaks an orphaned socket-polling timer.
static HUD_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Clone)]
struct App {
    id: String,
    name: String,
    mem: u64,
    swap: u64,
    frozen: bool,
    protected: bool,
    freezable: bool,
    hog: bool,
}

#[derive(Clone)]
struct Row {
    root: gtk::ListBoxRow,
    name: gtk::Label,
    meter: gtk::ProgressBar,
    mem: gtk::Label,
    chip: gtk::Label,
}

fn call(req: &serde_json::Value) -> Option<serde_json::Value> {
    let mut s = UnixStream::connect(SOCKET_PATH).ok()?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    writeln!(s, "{}", req).ok()?;
    let mut resp = String::new();
    s.read_to_string(&mut resp).ok()?;
    serde_json::from_str(resp.trim()).ok()
}

fn human_bytes(b: u64) -> String {
    const G: f64 = (1024 * 1024 * 1024) as f64;
    const M: f64 = (1024 * 1024) as f64;
    let bf = b as f64;
    if bf >= G {
        format!("{:.1}G", bf / G)
    } else if bf >= M {
        format!("{:.0}M", bf / M)
    } else {
        format!("{}B", b)
    }
}

/// Plain-language relative age for the activity trail.
fn ago_str(secs: u64) -> String {
    if secs < 45 {
        "just now".to_string()
    } else if secs < 5400 {
        format!("{}m ago", (secs + 30) / 60)
    } else {
        format!("{}h ago", (secs + 1800) / 3600)
    }
}

fn set_sev(w: &impl IsA<gtk::Widget>, sev: &str) {
    for c in ["good", "warn", "crit"] {
        w.remove_css_class(c);
    }
    w.add_css_class(sev);
}

fn share_sev(share: f64) -> &'static str {
    if share > 0.20 {
        "crit"
    } else if share > 0.10 {
        "warn"
    } else {
        "good"
    }
}

fn main() {
    // adw::Application initialises libadwaita (loads its stylesheet + accent /
    // dark handling). Step 1 of the migration keeps the existing gtk widgets and
    // CSS — the point is to verify our bespoke styling survives Adwaita's base
    // before adopting native chrome.
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run_with_args::<&str>(&[]);
}

const CSS: &str = "
.caption { font-size: 0.82em; opacity: 0.6; letter-spacing: 0.3px; }
.mono { font-family: monospace; font-feature-settings: 'tnum' 1; }
.hog .appname { font-weight: 800; }

/* meters — rounded with a little depth and smooth colour transitions */
progressbar.meter, progressbar.meter > trough, progressbar.meter > trough > progress { min-height: 7px; }
progressbar.meter > trough {
  border-radius: 6px;
  background: alpha(@theme_fg_color, 0.10);
}
progressbar.meter > trough > progress {
  border-radius: 6px;
  transition: background-color 400ms ease, box-shadow 400ms ease;
}
progressbar.meter.good > trough > progress { background-image: linear-gradient(to right, #56b84a, #6fd063); }
progressbar.meter.warn > trough > progress { background-image: linear-gradient(to right, #d99a33, #edb85a); }
progressbar.meter.crit > trough > progress {
  background-image: linear-gradient(to right, #e2504a, #f26a63);
  box-shadow: 0 0 7px alpha(#e2504a, 0.55);
}

/* list rows — breathing room, hover + accent selection, smooth */
.hud-list { background: transparent; }
.hud-list > row {
  border-radius: 10px;
  padding: 7px 12px;
  margin: 1px 2px;
  transition: background-color 160ms ease, opacity 300ms ease;
}
.hud-list > row:hover { background: alpha(@theme_fg_color, 0.05); }
.hud-list > row:selected {
  /* override the theme's solid-accent selection with a soft tint + edge bar */
  background-image: none;
  background-color: alpha(#e95420, 0.16);
  box-shadow: inset 3px 0 0 #e95420;
  color: @theme_fg_color;
}
/* frozen apps read as 'on ice' — dimmed + italic (a nod to the frost vision) */
.hud-list > row.frozen { opacity: 0.5; }
.hud-list > row.frozen .appname { font-style: italic; }

.pword { font-size: 1.05em; }
.pword.good { color: #6fd063; font-weight: 800; }
.pword.warn { color: #edb85a; font-weight: 800; }
.pword.crit { color: #f26a63; font-weight: 800; }

.chip { font-size: 0.72em; padding: 2px 9px; border-radius: 8px; font-weight: 600; }
.chip.live   { color: #7ad06e; background: alpha(#63c257, 0.14); }
.chip.paused { color: #ecc06a; background: alpha(#e0a53b, 0.18); }
.chip.pinned { color: #6fb8f0; background: alpha(#57a5e6, 0.18); }
.chip.locked { color: alpha(@theme_fg_color, 0.6); background: alpha(@theme_fg_color, 0.09); }

.actionbar {
  border-top: 1px solid alpha(@theme_fg_color, 0.08);
  margin-top: 4px;
  padding-top: 12px;
}
.actionbar .selname { font-weight: 600; }

/* the recent-activity trail: quiet, secondary, sits just under the card */
.activity {
  font-size: 0.80em;
  opacity: 0.55;
  margin: -2px 4px 2px 4px;
  letter-spacing: 0.2px;
}

/* header reads as a calm instrument panel, not a form */
.statuscard {
  padding: 12px 14px;
  border-radius: 14px;
  background-image: linear-gradient(to bottom,
    alpha(@theme_fg_color, 0.05), alpha(@theme_fg_color, 0.015));
  border: 1px solid alpha(@theme_fg_color, 0.06);
}
/* the hero sentence: quiet when all is well (calm tech), colour only on escalation */
.statusline {
  font-size: 1.28em;
  font-weight: 800;
  letter-spacing: 0.2px;
  margin-bottom: 2px;
  transition: color 500ms ease;
}
.statusline.good { color: alpha(@theme_fg_color, 0.88); }
.statusline.warn { color: #edb85a; }
.statusline.crit { color: #f26a63; }

/* frozen apps don't just dim — they slowly 'breathe' like they're asleep on ice
   (the vetoable-frost vision, brought into the HUD), with a faint cold tint */
@keyframes frostbreathe {
  0%   { opacity: 0.40; }
  50%  { opacity: 0.58; }
  100% { opacity: 0.40; }
}
.hud-list > row.frozen {
  animation: frostbreathe 3.2s ease-in-out infinite;
  background-image: linear-gradient(to right,
    alpha(#8fbfe6, 0.12), alpha(#8fbfe6, 0.02));
}
";

fn build_ui(app: &adw::Application) {
    // Re-summoning the HUD *recreates* the window rather than present()ing the
    // existing one. On Wayland, focusing an already-mapped toplevel requires an
    // xdg-activation token, and GNOME custom-keybindings spawn their command with
    // none (verified: XDG_ACTIVATION_TOKEN is always unset) — so present() can't
    // raise it. A freshly *mapped* toplevel, by contrast, gets initial focus.
    // The HUD is a stateless live view of the daemon, so tearing it down and
    // rebuilding is free and always lands focused. (A shell extension could focus
    // directly, but that needs a login the user has ruled out.)
    for win in app.windows() {
        win.destroy();
    }
    // Claim this build's generation; the old window's refresh timer sees a newer
    // value on its next tick and terminates itself.
    let my_gen =
        HUD_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;

    // Ask the daemon to keep us resident + OOM-protected so the control surface
    // stays operable exactly when it's needed most (under heavy pressure).
    let _ = call(&serde_json::json!({"cmd": "pin_self", "pid": std::process::id()}));

    // Install the stylesheet once per process — re-activations reuse it.
    static CSS_ONCE: std::sync::Once = std::sync::Once::new();
    CSS_ONCE.call_once(|| {
        let provider = gtk::CssProvider::new();
        provider.load_from_data(CSS);
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });

    let root = gtk::Box::new(gtk::Orientation::Vertical, 10);
    root.set_margin_top(12);
    root.set_margin_bottom(10);
    root.set_margin_start(12);
    root.set_margin_end(12);

    // ---- header: pressure + RAM + swap meters ----
    let (header, status, pword, pspark, pspark_data, ram_lbl, ram_meter, swap_lbl, swap_meter) =
        build_header();
    root.append(&header);

    // A muted trail of what rtux recently did — witnessed history is how trust
    // accrues (DESIGN.md). Hidden until there's something to show.
    let activity = gtk::Label::new(None);
    activity.add_css_class("caption");
    activity.add_css_class("activity");
    activity.set_xalign(0.0);
    activity.set_ellipsize(gtk::pango::EllipsizeMode::End);
    activity.set_visible(false);
    root.append(&activity);

    // ---- app list (boxed, scrolled) ----
    let list = gtk::ListBox::new();
    list.add_css_class("hud-list");
    list.set_selection_mode(gtk::SelectionMode::Single);

    let mems: Rc<RefCell<HashMap<String, u64>>> = Rc::new(RefCell::new(HashMap::new()));
    {
        let mems = mems.clone();
        list.set_sort_func(move |a, b| {
            let m = mems.borrow();
            let ma = m.get(a.widget_name().as_str()).copied().unwrap_or(0);
            let mb = m.get(b.widget_name().as_str()).copied().unwrap_or(0);
            mb.cmp(&ma).into()
        });
    }

    let frame = gtk::Frame::new(None);
    frame.set_child(Some(&list));
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(340)
        .vexpand(true)
        .child(&frame)
        .build();
    root.append(&scroller);

    // Freeze re-sorting while the pointer is inside, so rows hold still to click.
    let hovering = Rc::new(Cell::new(false));
    {
        let motion = gtk::EventControllerMotion::new();
        let h1 = hovering.clone();
        motion.connect_enter(move |_, _, _| h1.set(true));
        let h2 = hovering.clone();
        motion.connect_leave(move |_| h2.set(false));
        scroller.add_controller(motion);
    }

    // ---- bottom action bar (operates on the selected row) ----
    let (bar, sel_lbl, btn_pause, btn_cap, btn_pin, btn_close) = build_action_bar();
    root.append(&bar);

    // Native GNOME chrome: an integrated flat headerbar (window controls live in
    // it, no separate title bar) over the content, via AdwToolbarView. The
    // headerbar is flat so it blends into the instrument-panel look.
    let header_bar = adw::HeaderBar::new();
    header_bar.add_css_class("flat");
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header_bar);
    toolbar.set_content(Some(&root));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(560)
        .default_height(560)
        .content(&toolbar)
        .build();
    window.set_title(Some("pressured"));

    // Esc dismisses (daemon keeps running).
    {
        let key = gtk::EventControllerKey::new();
        let w = window.clone();
        key.connect_key_pressed(move |_, k, _, _| {
            if k == gtk::gdk::Key::Escape {
                w.close();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        window.add_controller(key);
    }

    // ---- shared state ----
    let rows: Rc<RefCell<HashMap<String, Row>>> = Rc::new(RefCell::new(HashMap::new()));
    let apps: Rc<RefCell<HashMap<String, App>>> = Rc::new(RefCell::new(HashMap::new()));
    let selected: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    // Reflect the selected app onto the action bar.
    let sync_bar: Rc<dyn Fn()> = {
        let apps = apps.clone();
        let selected = selected.clone();
        let sel_lbl = sel_lbl.clone();
        let btn_pause = btn_pause.clone();
        let btn_cap = btn_cap.clone();
        let btn_pin = btn_pin.clone();
        let btn_close = btn_close.clone();
        Rc::new(move || {
            let sel = selected.borrow().clone();
            match sel.and_then(|id| apps.borrow().get(&id).cloned()) {
                Some(a) => {
                    sel_lbl.set_text(&a.name);
                    btn_pause.set_label(if a.frozen { "Resume" } else { "Pause" });
                    btn_pause.set_tooltip_text(Some(if a.frozen {
                        "Resume (P): wake this paused app exactly where it left off."
                    } else {
                        "Pause (P): freeze this app so it stops running and can't use more memory. Reversible."
                    }));
                    btn_pause.set_sensitive(a.freezable || a.frozen);
                    btn_cap.set_sensitive(a.freezable);
                    btn_pin.set_label(if a.protected { "Unpin" } else { "Pin" });
                    btn_pin.set_tooltip_text(Some(if a.protected {
                        "Unpin (I): stop keeping this app in fast memory."
                    } else {
                        "Pin (I): keep this app in fast memory so it always stays quick."
                    }));
                    btn_pin.set_sensitive(true);
                    btn_close.set_sensitive(a.freezable);
                }
                None => {
                    sel_lbl.set_text("Select an app to act on it");
                    for b in [&btn_pause, &btn_cap, &btn_pin, &btn_close] {
                        b.set_sensitive(false);
                    }
                }
            }
        })
    };

    {
        let selected = selected.clone();
        let sync_bar = sync_bar.clone();
        list.connect_row_selected(move |_, row| {
            *selected.borrow_mut() = row.map(|r| r.widget_name().to_string());
            sync_bar();
        });
    }

    // ---- refresh: update everything in place ----
    let refresh: Rc<dyn Fn()> = {
        let list = list.clone();
        let rows = rows.clone();
        let apps = apps.clone();
        let mems = mems.clone();
        let selected = selected.clone();
        let hovering = hovering.clone();
        let sync_bar = sync_bar.clone();
        let status = status.clone();
        let activity = activity.clone();
        let pword = pword.clone();
        let pspark = pspark.clone();
        let pspark_data = pspark_data.clone();
        let ram_lbl = ram_lbl.clone();
        let ram_meter = ram_meter.clone();
        let swap_lbl = swap_lbl.clone();
        let swap_meter = swap_meter.clone();
        Rc::new(move || {
            let reply = match call(&serde_json::json!({"cmd": "list"})) {
                Some(r) => r,
                None => {
                    pword.set_text("daemon unreachable");
                    set_sev(&pword, "crit");
                    return;
                }
            };

            // header
            let some = reply["some_avg10"].as_f64().unwrap_or(0.0);
            let full = reply["full_avg10"].as_f64().unwrap_or(0.0);
            let (pw, psev) = if some > 25.0 || full > 10.0 {
                ("critical", "crit")
            } else if some > 5.0 {
                ("elevated", "warn")
            } else {
                ("normal", "good")
            };
            pword.set_text(pw);
            set_sev(&pword, psev);
            // Feed the pressure sparkline the rolling history from the daemon
            // (falls back to just the current sample if the daemon is older).
            {
                let mut buf = pspark_data.borrow_mut();
                match reply["pressure_trend"].as_array() {
                    Some(arr) if !arr.is_empty() => {
                        buf.clear();
                        buf.extend(arr.iter().filter_map(|v| v.as_f64()));
                    }
                    // Older daemon with no history field: accumulate our own so
                    // the trace still fills while the window stays open.
                    _ => {
                        buf.push(some);
                        let len = buf.len();
                        if len > 60 {
                            buf.drain(0..len - 60);
                        }
                    }
                }
            }
            pspark.queue_draw();

            let mem_total = reply["mem_total"].as_u64().unwrap_or(1).max(1);
            let mem_used = reply["mem_used"].as_u64().unwrap_or(0);
            let swap_total = reply["swap_total"].as_u64().unwrap_or(0);
            let swap_used = reply["swap_used"].as_u64().unwrap_or(0);
            ram_lbl.set_text(&format!("{} / {}", human_bytes(mem_used), human_bytes(mem_total)));
            let ram_frac = mem_used as f64 / mem_total as f64;
            ram_meter.set_fraction(ram_frac.clamp(0.0, 1.0));
            set_sev(&ram_meter, if ram_frac > 0.9 { "crit" } else if ram_frac > 0.75 { "warn" } else { "good" });
            if swap_total > 0 {
                swap_lbl.set_text(&format!("{} / {}", human_bytes(swap_used), human_bytes(swap_total)));
                let sf = swap_used as f64 / swap_total as f64;
                swap_meter.set_fraction(sf.clamp(0.0, 1.0));
                set_sev(&swap_meter, if sf > 0.8 { "crit" } else if sf > 0.5 { "warn" } else { "good" });
            } else {
                swap_lbl.set_text("none");
                swap_meter.set_fraction(0.0);
            }

            // apps
            let parsed = parse_apps(&reply);

            // Truthful status hero: reflect what rtux has actually done. A paused
            // count comes straight from live state — never a canned "handling it".
            // When apps are paused the machine is *coping*, so it reads calm
            // (amber "handled"), not alarm-red; genuine red is high pressure with
            // nothing paused yet.
            let paused = parsed.iter().filter(|a| a.frozen).count();
            let (sentence, ssev) = if paused > 0 {
                (
                    format!(
                        "Paused {} background app{} to keep you fast",
                        paused,
                        if paused == 1 { "" } else { "s" }
                    ),
                    "warn",
                )
            } else if psev == "crit" {
                ("Under heavy load — protecting your foreground".to_string(), "crit")
            } else if psev == "warn" {
                ("Getting busy — keeping the foreground quick".to_string(), "warn")
            } else {
                ("Running smoothly".to_string(), "good")
            };
            status.set_text(&sentence);
            set_sev(&status, ssev);

            // Recent activity trail: "Paused Chrome · 2m ago   ·   Resumed …".
            if let Some(evs) = reply["recent"].as_array().filter(|a| !a.is_empty()) {
                let fmt = |e: &serde_json::Value| {
                    let text = e["text"].as_str().unwrap_or("");
                    let ago = ago_str(e["ago_secs"].as_u64().unwrap_or(0));
                    format!("{} · {}", text, ago)
                };
                let line = evs.iter().take(3).map(&fmt).collect::<Vec<_>>().join("    ·    ");
                activity.set_text(&format!("⟲  {}", line));
                // Full history on hover, one per line.
                let full = evs.iter().map(&fmt).collect::<Vec<_>>().join("\n");
                activity.set_tooltip_text(Some(&full));
                activity.set_visible(true);
            } else {
                activity.set_visible(false);
            }

            let mut seen: HashSet<String> = HashSet::new();
            for a in &parsed {
                seen.insert(a.id.clone());
                mems.borrow_mut().insert(a.id.clone(), a.mem);
                apps.borrow_mut().insert(a.id.clone(), a.clone());
                let mut rows_m = rows.borrow_mut();
                if let Some(row) = rows_m.get(&a.id) {
                    update_row(row, a, mem_total);
                } else {
                    let row = build_row(a, mem_total);
                    list.append(&row.root);
                    rows_m.insert(a.id.clone(), row);
                }
            }

            // drop vanished apps
            let gone: Vec<String> = rows
                .borrow()
                .keys()
                .filter(|k| !seen.contains(*k))
                .cloned()
                .collect();
            for id in gone {
                if let Some(row) = rows.borrow_mut().remove(&id) {
                    list.remove(&row.root);
                }
                mems.borrow_mut().remove(&id);
                apps.borrow_mut().remove(&id);
                if selected.borrow().as_deref() == Some(id.as_str()) {
                    *selected.borrow_mut() = None;
                }
            }

            if !hovering.get() {
                list.invalidate_sort();
            }
            sync_bar();
        })
    };

    // Wire action buttons.
    let act = {
        let selected = selected.clone();
        let apps = apps.clone();
        let refresh = refresh.clone();
        move |action: &str| {
            let Some(id) = selected.borrow().clone() else { return };
            // pause/pin are stateful toggles resolved from current app state
            let resolved = match action {
                "pause" => {
                    let frozen = apps.borrow().get(&id).map(|a| a.frozen).unwrap_or(false);
                    if frozen { "thaw" } else { "freeze" }
                }
                "pin" => {
                    let prot = apps.borrow().get(&id).map(|a| a.protected).unwrap_or(false);
                    if prot { "unprotect" } else { "protect" }
                }
                other => other,
            };
            let _ = call(&serde_json::json!({"cmd": "act", "action": resolved, "id": id}));
            refresh();
        }
    };
    let act = Rc::new(act);
    for (btn, name) in [
        (&btn_pause, "pause"),
        (&btn_cap, "cap"),
        (&btn_pin, "pin"),
        (&btn_close, "kill"),
    ] {
        let act = act.clone();
        let name = name.to_string();
        btn.connect_clicked(move |_| act(&name));
    }

    // Keyboard shortcuts for the actions (operate on the selected row; the `act`
    // closure no-ops if nothing is selected). Arrow keys navigate the list
    // natively; these drive the action bar without reaching for the mouse.
    {
        let act = act.clone();
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(move |_, k, _, _| {
            use gtk::gdk::Key;
            let hit = match k {
                Key::p | Key::P => Some("pause"),
                Key::c | Key::C => Some("cap"),
                Key::i | Key::I => Some("pin"),
                // Close is the one irreversible action — deliberate Delete only,
                // never a bare letter that could be fat-fingered.
                Key::Delete => Some("kill"),
                _ => None,
            };
            match hit {
                Some(a) => {
                    act(a);
                    glib::Propagation::Stop
                }
                None => glib::Propagation::Proceed,
            }
        });
        window.add_controller(keys);
    }

    // initial paint + 1s live refresh
    refresh();
    // Preselect the biggest consumer so the HUD is usable by keyboard at once.
    if let Some(row) = list.row_at_index(0) {
        list.select_row(Some(&row));
    }
    {
        let refresh = refresh.clone();
        glib::timeout_add_seconds_local(1, move || {
            // A newer window superseded us — stop polling and drop our widgets.
            if HUD_GENERATION.load(std::sync::atomic::Ordering::SeqCst) != my_gen {
                return glib::ControlFlow::Break;
            }
            refresh();
            glib::ControlFlow::Continue
        });
    }

    window.present();
    list.grab_focus();
}

fn build_header() -> (
    gtk::Box,
    gtk::Label,
    gtk::Label,
    gtk::DrawingArea,
    Rc<RefCell<Vec<f64>>>,
    gtk::Label,
    gtk::ProgressBar,
    gtk::Label,
    gtk::ProgressBar,
) {
    let header = gtk::Box::new(gtk::Orientation::Vertical, 6);
    header.add_css_class("statuscard");

    // The hero: a plain-language sentence of what the machine is doing right now.
    // Always visible (not hidden in a tooltip) — legibility is the whole point,
    // and it only ever states what's actually true (paused counts come from live
    // app state, never a canned claim).
    let status = gtk::Label::new(Some("Checking…"));
    status.add_css_class("statusline");
    status.set_xalign(0.0);
    status.set_wrap(true);
    header.append(&status);

    // pressure line
    let prow = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let ptitle = gtk::Label::new(Some("Pressure"));
    ptitle.add_css_class("caption");
    ptitle.set_width_request(58);
    ptitle.set_xalign(0.0);
    let pword = gtk::Label::new(Some("—"));
    pword.add_css_class("pword");
    pword.set_width_request(64);
    pword.set_xalign(0.0);
    // A sparkline of the last ~minute of pressure instead of a bare bar, so you
    // can see it *climbing* toward trouble — the anticipatory read.
    let (pspark, pspark_data) = sparkline();
    prow.append(&ptitle);
    prow.append(&pword);
    prow.append(&pspark);
    prow.set_tooltip_text(Some(
        "How hard your computer is working to find free memory, over the last \
         minute. The faint line marks the level where it would start to stutter — \
         so if the trace is climbing toward it, rtux is about to step in.",
    ));
    header.append(&prow);

    let (rrow, ram_lbl, ram_meter) = meter_row("RAM");
    rrow.set_tooltip_text(Some(
        "Fast memory — in use / total. When it fills up, apps slow down or get paused.",
    ));
    let (srow, swap_lbl, swap_meter) = meter_row("Swap");
    srow.set_tooltip_text(Some(
        "Backup memory, used when the fast memory fills up. Some is compressed and \
         still fairly quick; some lives on disk and is slow. In use / total.",
    ));
    header.append(&rrow);
    header.append(&srow);

    (header, status, pword, pspark, pspark_data, ram_lbl, ram_meter, swap_lbl, swap_meter)
}

/// A pressure sparkline: a filled trace of recent `some.avg10` (percent) over a
/// fixed 0–50 ceiling, with a faint guide at the critical threshold (25). Colour
/// follows the current level. Reads the shared data buffer on each redraw.
fn sparkline() -> (gtk::DrawingArea, Rc<RefCell<Vec<f64>>>) {
    let data: Rc<RefCell<Vec<f64>>> = Rc::new(RefCell::new(Vec::new()));
    let area = gtk::DrawingArea::new();
    area.set_hexpand(true);
    area.set_content_height(24);
    area.set_valign(gtk::Align::Center);
    let d = data.clone();
    area.set_draw_func(move |_, cr, w, h| {
        let data = d.borrow();
        let w = w as f64;
        let h = h as f64;
        const CEIL: f64 = 50.0; // avg10 percent; 25 = critical
        // Inset so a flat, near-zero trace still sits visibly above the bottom
        // edge rather than vanishing into it.
        let pad = 3.0;
        let plot_h = (h - 2.0 * pad).max(1.0);
        let y = |v: f64| pad + plot_h * (1.0 - (v / CEIL).clamp(0.0, 1.0));
        // Faint chart panel so the sparkline reads as *present* even when pressure
        // is flat-zero (which is most of the time — that's the good state).
        cr.set_source_rgba(0.5, 0.5, 0.5, 0.07);
        cr.rectangle(0.0, 0.0, w, h);
        let _ = cr.fill();
        // critical-threshold guide
        cr.set_line_width(1.0);
        cr.set_source_rgba(0.5, 0.5, 0.5, 0.22);
        cr.move_to(0.0, y(25.0));
        cr.line_to(w, y(25.0));
        let _ = cr.stroke();
        if data.len() < 2 {
            return;
        }
        let n = data.len();
        let x = |i: usize| w * (i as f64) / ((n - 1) as f64);
        let cur = *data.last().unwrap();
        let (r, g, b) = if cur > 25.0 {
            (0.886, 0.314, 0.290) // crit
        } else if cur > 5.0 {
            (0.929, 0.722, 0.353) // warn
        } else {
            (0.337, 0.722, 0.290) // good
        };
        // filled area under the trace (down to the inset baseline)
        let base = y(0.0);
        cr.move_to(0.0, base);
        for (i, v) in data.iter().enumerate() {
            cr.line_to(x(i), y(*v));
        }
        cr.line_to(w, base);
        cr.close_path();
        cr.set_source_rgba(r, g, b, 0.18);
        let _ = cr.fill();
        // the trace itself
        cr.move_to(x(0), y(data[0]));
        for (i, v) in data.iter().enumerate().skip(1) {
            cr.line_to(x(i), y(*v));
        }
        cr.set_source_rgba(r, g, b, 0.95);
        cr.set_line_width(1.8);
        let _ = cr.stroke();
    });
    (area, data)
}

fn meter_row(caption: &str) -> (gtk::Box, gtk::Label, gtk::ProgressBar) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let cap = gtk::Label::new(Some(caption));
    cap.add_css_class("caption");
    cap.set_width_request(58);
    cap.set_xalign(0.0);
    let val = gtk::Label::new(Some("—"));
    val.add_css_class("mono");
    val.add_css_class("caption");
    val.set_width_request(120);
    val.set_xalign(1.0);
    let m = meter();
    row.append(&cap);
    row.append(&val);
    row.append(&m);
    (row, val, m)
}

fn meter() -> gtk::ProgressBar {
    let pb = gtk::ProgressBar::new();
    pb.set_show_text(false);
    pb.set_hexpand(true);
    pb.set_valign(gtk::Align::Center);
    pb.add_css_class("meter");
    pb.add_css_class("good");
    pb
}

fn build_action_bar() -> (
    gtk::Box,
    gtk::Label,
    gtk::Button,
    gtk::Button,
    gtk::Button,
    gtk::Button,
) {
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    bar.add_css_class("actionbar");
    let sel = gtk::Label::new(Some("Select an app to act on it"));
    sel.add_css_class("selname");
    sel.set_hexpand(true);
    sel.set_xalign(0.0);
    sel.set_ellipsize(pango::EllipsizeMode::End);
    sel.set_tooltip_text(Some(
        "The app the buttons on the right will act on. Click a row above to choose it.",
    ));
    let pin = gtk::Button::with_label("Pin");
    let cap = gtk::Button::with_label("Cap");
    let pause = gtk::Button::with_label("Pause");
    let close = gtk::Button::with_label("Close");
    close.add_css_class("destructive-action");
    // A gap sets the one irreversible action apart from the reversible ones.
    close.set_margin_start(10);

    // Ordered by escalating consequence — protect → slow → stop → end. Each
    // carries a plain-language tooltip + accessible description (the action
    // words alone aren't self-evident).
    let helps = [
        (&pin, "Pin (I): keep this app in fast memory so it always stays quick — for something you don't want slowed or paused. The gentlest action."),
        (&cap, "Cap (C): gently slow this app so it gives back memory it isn't using — without pausing it."),
        (&pause, "Pause (P): freeze this app so it stops running and can't use more memory. Reversible — Resume wakes it exactly where it was."),
        (&close, "Close (Delete): quit this app. The only action here you can't undo."),
    ];
    for (b, text) in helps {
        b.set_tooltip_text(Some(text));
        b.update_property(&[gtk::accessible::Property::Description(text)]);
        b.set_sensitive(false);
    }
    bar.append(&sel);
    bar.append(&pin);
    bar.append(&cap);
    bar.append(&pause);
    bar.append(&close);
    (bar, sel, pause, cap, pin, close)
}

fn parse_apps(reply: &serde_json::Value) -> Vec<App> {
    let empty = vec![];
    reply["apps"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .map(|a| App {
            id: a["id"].as_str().unwrap_or("").to_string(),
            name: a["name"].as_str().unwrap_or("?").to_string(),
            mem: a["mem_bytes"].as_u64().unwrap_or(0),
            swap: a["swap_bytes"].as_u64().unwrap_or(0),
            frozen: a["frozen"].as_bool().unwrap_or(false),
            protected: a["protected"].as_bool().unwrap_or(false),
            freezable: a["freezable"].as_bool().unwrap_or(false),
            hog: a["flagged"].as_str() == Some("top_consumer"),
        })
        .collect()
}

fn build_row(a: &App, mem_total: u64) -> Row {
    let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 10);

    let name = gtk::Label::new(None);
    name.add_css_class("appname");
    name.set_hexpand(true);
    name.set_xalign(0.0);
    name.set_ellipsize(pango::EllipsizeMode::End);
    name.set_tooltip_text(Some(
        "The app. Terminal windows show what's running inside them and the folder \
         it's in. ▲ marks the biggest app rtux can pause — the first it would \
         pause under pressure (not necessarily the biggest overall, since your \
         terminals and system apps are off-limits). ❄ marks one rtux has paused.",
    ));

    let meter = gtk::ProgressBar::new();
    meter.set_show_text(false);
    meter.set_valign(gtk::Align::Center);
    meter.set_width_request(96);
    meter.add_css_class("meter");
    meter.set_tooltip_text(Some("The share of your total memory this app is using."));

    let mem = gtk::Label::new(None);
    mem.add_css_class("mono");
    mem.set_width_request(120);
    mem.set_xalign(1.0);
    mem.set_tooltip_text(Some(
        "Memory this app is using now. \"+N\" is how much of it was moved to backup \
         memory to free up space.",
    ));

    let chip = gtk::Label::new(None);
    chip.add_css_class("chip");
    chip.set_width_request(64);

    hbox.append(&name);
    hbox.append(&meter);
    hbox.append(&mem);
    hbox.append(&chip);

    let root = gtk::ListBoxRow::new();
    root.set_widget_name(&a.id);
    root.set_child(Some(&hbox));

    let row = Row { root, name, meter, mem, chip };
    update_row(&row, a, mem_total);
    row
}

fn update_row(row: &Row, a: &App, mem_total: u64) {
    // Frozen state wins the marker (it's the more important thing to see); the
    // ❄ + dim/italic row makes a paused app read as "on ice", never "broken".
    let label = if a.frozen {
        format!("❄ {}", a.name)
    } else if a.hog {
        format!("▲ {}", a.name)
    } else {
        a.name.clone()
    };
    row.name.set_text(&label);
    if a.hog {
        row.root.add_css_class("hog");
    } else {
        row.root.remove_css_class("hog");
    }
    if a.frozen {
        row.root.add_css_class("frozen");
    } else {
        row.root.remove_css_class("frozen");
    }

    let share = a.mem as f64 / mem_total as f64;
    row.meter.set_fraction(share.clamp(0.0, 1.0));
    set_sev(&row.meter, share_sev(share));

    let mem_txt = if a.swap > 0 {
        format!("{}  +{}", human_bytes(a.mem), human_bytes(a.swap))
    } else {
        human_bytes(a.mem)
    };
    row.mem.set_text(&mem_txt);

    let (chip_txt, chip_cls, chip_help) = if a.frozen {
        ("paused", "paused", "Paused by rtux — frozen and not running. Resume wakes it up.")
    } else if a.protected {
        ("pinned", "pinned", "Pinned — kept in fast memory so it stays quick.")
    } else if !a.freezable {
        ("system", "locked", "A protected system app — rtux won't slow, pause, or close it.")
    } else {
        ("live", "live", "Running normally — you can pin, slow, pause, or close it.")
    };
    row.chip.set_text(chip_txt);
    row.chip.set_tooltip_text(Some(chip_help));
    for c in ["live", "paused", "pinned", "locked"] {
        row.chip.remove_css_class(c);
    }
    row.chip.add_css_class(chip_cls);

    // Screen-reader summary for the whole row.
    let swap_part = if a.swap > 0 {
        format!(", {} in swap", human_bytes(a.swap))
    } else {
        String::new()
    };
    row.root.update_property(&[gtk::accessible::Property::Label(&format!(
        "{}, {}{}, {}",
        a.name,
        human_bytes(a.mem),
        swap_part,
        chip_txt
    ))]);
}
