// rtux pressure indicator + attention feed.
//
// Two jobs, both of which only the shell can do:
//   1. An ambient top-bar memory-pressure light. Reads /proc/pressure/memory
//      ASYNCHRONOUSLY on a 2s timer, so it never blocks the shell (a
//      responsiveness tool must never be the thing that stalls the compositor).
//   2. Attention-following. The shell is the *only* thing that can see which
//      window has focus on Wayland — org.gnome.Shell.Introspect denies it to
//      unprivileged clients, and AT-SPI is off. So on every focus change we read
//      the focused window's PID (get_focus_window().get_pid(), free inside the
//      shell) and hand it to the daemon over /run/pressured.sock, which pins that
//      app's pages resident (clamped) and relaxes the previous one. That's what
//      makes the app you're actually using stay quick under pressure.

import St from 'gi://St';
import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import Clutter from 'gi://Clutter';
import GObject from 'gi://GObject';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

const POLL_SECONDS = 2;
const SOCKET_PATH = '/run/pressured.sock';
// New-client-per-summon: Wayland only focuses a fresh client (see the daemon's
// mitigate::SUMMON_HUD). Kill any running HUD, then spawn one.
const SUMMON_HUD =
    "pkill -KILL -x pressured-hud; for i in $(seq 50); do pgrep -x pressured-hud >/dev/null || break; sleep 0.02; done; pressured-hud";

function classify(psi) {
    let some = 0, full = 0;
    for (const line of psi.split('\n')) {
        const m = line.match(/avg10=([\d.]+)/);
        if (!m)
            continue;
        if (line.startsWith('some'))
            some = parseFloat(m[1]);
        else if (line.startsWith('full'))
            full = parseFloat(m[1]);
    }
    if (some > 25 || full > 10)
        return 'critical';
    if (some > 5)
        return 'elevated';
    return 'normal';
}

const Indicator = GObject.registerClass(
class RtuxIndicator extends PanelMenu.Button {
    _init() {
        super._init(0.0, 'rtux', true); // dontCreateMenu — clicks launch the HUD
        this._dot = new St.Label({
            text: '●', // ●
            y_align: Clutter.ActorAlign.CENTER,
            style_class: 'rtux-dot rtux-good',
        });
        this.add_child(this._dot);

        this.connect('button-press-event', () => {
            try {
                GLib.spawn_command_line_async(`sh -c ${GLib.shell_quote(SUMMON_HUD)}`);
            } catch (e) {
                logError(e, 'rtux: failed to launch pressured-hud');
            }
            return Clutter.EVENT_STOP;
        });
    }

    setLevel(level) {
        for (const c of ['rtux-good', 'rtux-warn', 'rtux-crit'])
            this._dot.remove_style_class_name(c);
        this._dot.add_style_class_name(
            level === 'critical' ? 'rtux-crit'
            : level === 'elevated' ? 'rtux-warn'
            : 'rtux-good');
    }
});

export default class RtuxExtension extends Extension {
    enable() {
        this._indicator = new Indicator();
        Main.panel.addToStatusArea('rtux', this._indicator, 0, 'right');
        this._tick();
        this._timer = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, POLL_SECONDS, () => {
            this._tick();
            return GLib.SOURCE_CONTINUE;
        });

        // Attention-following: tell the daemon which app has focus so it can keep
        // that one resident under pressure. Fire on every focus change, plus once
        // now for whatever's already focused.
        this._lastPid = 0;
        this._focusId = global.display.connect(
            'notify::focus-window', () => this._onFocus());
        this._onFocus();
    }

    disable() {
        if (this._timer) {
            GLib.source_remove(this._timer);
            this._timer = null;
        }
        if (this._focusId) {
            global.display.disconnect(this._focusId);
            this._focusId = null;
        }
        this._indicator?.destroy();
        this._indicator = null;
    }

    _onFocus() {
        const win = global.display.get_focus_window?.();
        if (!win)
            return;
        let pid = 0;
        try {
            pid = win.get_pid();
        } catch (e) {
            return;
        }
        // Skip repeats (focus can notify several times for one switch) and our
        // own HUD (already pinned; foregrounding it would just churn).
        if (pid <= 0 || pid === this._lastPid)
            return;
        this._lastPid = pid;
        this._sendForeground(pid);
    }

    // Hand the focused PID to the daemon over the control socket, fully async so
    // a slow or absent daemon can never stall the compositor.
    _sendForeground(pid) {
        let client;
        try {
            client = new Gio.SocketClient();
        } catch (e) {
            return;
        }
        const addr = new Gio.UnixSocketAddress({path: SOCKET_PATH});
        client.connect_async(addr, null, (c, res) => {
            let conn;
            try {
                conn = c.connect_finish(res);
            } catch (e) {
                return; // daemon down — attention-following is best-effort
            }
            try {
                const msg = JSON.stringify({cmd: 'foreground', pid}) + '\n';
                conn.get_output_stream().write_all(
                    new TextEncoder().encode(msg), null);
                conn.close(null);
            } catch (e) {
                // ignore write/close failures — nothing to recover
            }
        });
    }

    _tick() {
        const file = Gio.File.new_for_path('/proc/pressure/memory');
        file.load_contents_async(null, (f, res) => {
            let level = 'normal';
            try {
                const [ok, contents] = f.load_contents_finish(res);
                if (ok)
                    level = classify(new TextDecoder().decode(contents));
            } catch (e) {
                // transient read failure — keep the last colour
            }
            this._indicator?.setLevel(level);
        });
    }
}
