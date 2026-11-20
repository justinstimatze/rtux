// rtux pressure indicator + attention feed.
//
// Two jobs, both of which only the shell can do:
//   1. An ambient top-bar memory-pressure light. Every 2s it asks the daemon
//      for the level over /run/pressured.sock, ASYNCHRONOUSLY so it never blocks
//      the shell (a responsiveness tool must never be the thing that stalls the
//      compositor). It asks the daemon rather than reading /proc itself so the
//      dot doubles as a heartbeat: if the round trip fails, the dot turns into a
//      hollow grey ○ within one tick. When it read /proc directly it stayed green
//      through a 26-hour outage in which every client was refused.
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

// Give up on a ping after this long: a daemon that can't answer within 1.5s is
// as unreachable as one that refuses, from the user's side.
const PING_TIMEOUT_MS = 1500;
const LEVELS = ['normal', 'elevated', 'critical'];

const Indicator = GObject.registerClass(
class RtuxIndicator extends PanelMenu.Button {
    _init() {
        super._init(0.0, 'rtux', true); // dontCreateMenu — clicks launch the HUD
        this._liveLevel = 'normal';
        // Latched-witness state: set the first time pressure goes critical, held
        // until the user opens the HUD. See setLevel().
        this._latched = false;
        this._dot = new St.Label({
            text: '●',
            y_align: Clutter.ActorAlign.CENTER,
            style_class: 'rtux-dot rtux-good',
        });
        this.add_child(this._dot);

        this.connect('button-press-event', () => {
            // Opening the HUD is the acknowledgement — clear the witness mark.
            this.clearLatch();
            try {
                GLib.spawn_command_line_async(`sh -c ${GLib.shell_quote(SUMMON_HUD)}`);
            } catch (e) {
                logError(e, 'rtux: failed to launch pressured-hud');
            }
            return Clutter.EVENT_STOP;
        });
    }

    // The daemon could not be reached. Hollow grey until the next good ping;
    // the reason goes to the shell's journal once per outage, not every tick.
    setDisconnected(reason) {
        if (this._offline === undefined || this._offline === null)
            console.warn(`rtux: can't reach pressured at ${SOCKET_PATH}: ${reason}`);
        this._offline = reason;
        this._render();
    }

    setLevel(level) {
        if (this._offline) {
            console.log('rtux: reached pressured again');
            this._offline = null;
        }
        if (!LEVELS.includes(level)) {
            this._render();
            return;
        }
        this._liveLevel = level;
        // Latch a witness the first time pressure goes critical. Even after PSI
        // falls back — and especially under Do-Not-Disturb, when no banner and no
        // drawer entry fire — the dot must keep a visible record that the machine
        // intervened, until the user acknowledges it by opening the HUD. Without
        // this the dot snaps back to calm green the instant pressure clears, so a
        // *completed* freeze leaves no trace at all (gh #1).
        if (level === 'critical')
            this._latched = true;
        this._render();
    }

    clearLatch() {
        if (!this._latched)
            return;
        this._latched = false;
        this._render();
    }

    _render() {
        for (const c of ['rtux-good', 'rtux-warn', 'rtux-crit', 'rtux-off', 'rtux-latched'])
            this._dot.remove_style_class_name(c);
        if (this._offline) {
            this._dot.add_style_class_name('rtux-off');
            this._dot.set_text('○');
            return;
        }
        this._dot.add_style_class_name(
            this._liveLevel === 'critical' ? 'rtux-crit'
            : this._liveLevel === 'elevated' ? 'rtux-warn'
            : 'rtux-good');
        // A latched witness shows a ringed dot (◉): the colour still tracks
        // current pressure (green again once it's calm), but the ring says "I did
        // something while you weren't looking — click to see." Plain ● otherwise.
        this._dot.set_text(this._latched ? '◉' : '●');
        if (this._latched)
            this._dot.add_style_class_name('rtux-latched');
        else
            this._dot.remove_style_class_name('rtux-latched');
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
        this._lastWindow = 0;
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
        // Dedup on the WINDOW, not the pid.
        //
        // focus-window notifies several times per switch, so some dedup is needed —
        // but keying it on the pid silently drops the most common switch there is.
        // One terminal process owns every one of its windows, so alt-tabbing from one
        // terminal window to another produced a repeat pid and sent the daemon
        // NOTHING — on a box with dozens of terminal windows open, that is the
        // majority of all window switches. The daemon cannot thaw
        // what it is never told about, so a paused session in the window you just
        // switched to stayed paused until the pressure episode ended on its own —
        // measured 2026-07-22 as 6 focus-thaws against ~69 freeze cycles.
        //
        // The window id distinguishes those; the pid is still what we send, since
        // that is what the daemon resolves to a cgroup.
        const wid = win.get_id?.() ?? pid;
        if (pid <= 0 || wid === this._lastWindow)
            return;
        this._lastWindow = wid;
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

    // Ask the daemon for the pressure level. Any failure along the way (connect
    // refused, EACCES, no reply, a reply that isn't a level) means the round trip
    // is broken, and the dot says so. One ping in flight at a time, and none
    // outlives PING_TIMEOUT_MS, so a wedged daemon can't pile up connections.
    _tick() {
        if (this._pinging)
            return;
        this._pinging = true;
        const cancel = new Gio.Cancellable();
        let timedOut = false;
        const timer = GLib.timeout_add(GLib.PRIORITY_DEFAULT, PING_TIMEOUT_MS, () => {
            timedOut = true;
            cancel.cancel();
            return GLib.SOURCE_REMOVE;
        });
        const done = (level, err) => {
            if (!timedOut)
                GLib.source_remove(timer);
            this._pinging = false;
            if (level)
                this._indicator?.setLevel(level);
            else
                this._indicator?.setDisconnected(timedOut ? 'no reply within 1.5s' : err);
        };

        let client;
        try {
            client = new Gio.SocketClient();
        } catch (e) {
            done(null, e.message);
            return;
        }
        const addr = new Gio.UnixSocketAddress({path: SOCKET_PATH});
        client.connect_async(addr, cancel, (c, res) => {
            let conn;
            try {
                conn = c.connect_finish(res);
                conn.get_output_stream().write_all(
                    new TextEncoder().encode('{"cmd":"ping"}\n'), cancel);
            } catch (e) {
                try {
                    conn?.close(null);
                } catch (_) {}
                done(null, e.message);
                return;
            }
            const input = new Gio.DataInputStream({base_stream: conn.get_input_stream()});
            input.read_line_async(GLib.PRIORITY_DEFAULT, cancel, (s, r) => {
                let level = null;
                let err = 'empty reply';
                try {
                    const [line] = s.read_line_finish_utf8(r);
                    const reply = line ? JSON.parse(line) : null;
                    if (reply?.ok && reply.level)
                        level = reply.level;
                    else if (reply)
                        // An older daemon answers "bad request": reachable, but it
                        // predates the heartbeat, so the dot can't trust it.
                        err = reply.msg ?? 'unexpected reply';
                } catch (e) {
                    err = e.message;
                }
                try {
                    conn.close(null);
                } catch (_) {}
                done(level, err);
            });
        });
    }
}
