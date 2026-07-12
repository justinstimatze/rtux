# rtux

*Here, in the dim glow of the laptop, we observe a creature in crisis.*

The machine — magnificent, capable, barely two years old — has been overwhelmed.
Its cursor stutters across the screen, lagging seconds behind the hand that moves
it. Windows freeze mid-gesture. The whole system, so responsive only moments ago,
now gasps for breath.

In the wild, this is where lesser systems perish — or where the human, in
frustration, reaches for the power button.

But watch closely. Something stirs at the edge of the frame.

**rtux** — *the program is called `pressured`; `rtux` is the species* — is a small,
patient creature that lives in the background of your Linux desktop. When memory
runs short it keeps the machine feeling as fast as it truly is by *pausing* the
runaway app instead of killing it — reversibly — and, remarkably, telling you
plainly what it did, then undoing it the moment the danger passes. It runs as a
small `systemd` service and needs only systemd + cgroups v2 (Ubuntu, Fedora, Arch,
and most others).

**To bring it to your own machine** — a modern GNOME desktop:

```sh
sudo apt install libgtk-4-dev libadwaita-1-dev   # for the control window (Ubuntu 24.04+)
cargo build --release --features hud,tray
sudo ./install.sh          # raises the guardian, offers to enable zram
./setup-hotkey.sh          # Ctrl+Alt+P summons the control window
./install-extension.sh     # the top-bar light (wakes at your next login)
```

On an older or non-Ubuntu habitat, see [Bringing it home](#bringing-it-home)
below. The full field journal — *why* any of this works — is [DESIGN.md](DESIGN.md).

## The phenomenon: why a fast machine suddenly crawls

Every computer has a fixed amount of *fast* memory — RAM. Open enough browser
tabs, editors, and chat windows, and it fills.

Here is the cruel part. When it fills, Linux does not quietly ask the greediest
application to wait its turn. It begins shuffling *everyone's* memory onto the
slow disk — evenly, indiscriminately — including the very software that draws your
cursor. And so a machine that could handle the work with ease instead grinds to a
crawl, and feels cheap and old, when it is neither.

This is not a hardware failure. It is a *choice* about who gets served first. A
cheap phone stays perfectly smooth in the same moment, because it ruthlessly
protects whatever you are looking at. rtux teaches your Linux desktop the same
good manners.

## The intervention

When memory runs short, rtux acts — gently, reversibly, and always in the open:

1. **It shields the things that draw your screen.** The cursor and the desktop
   itself are kept in fast memory no matter what, so they never stutter. Whatever
   else is struggling, your hand and the pointer stay in step.
2. **It has a quiet word with the greediest app.** First it simply asks that app
   to hand back memory it isn't really using. If the pressure keeps climbing, it
   *pauses* the app outright — freezing it whole and intact, like an animal in
   torpor. Nothing is lost; its tabs and its work wait exactly where you left them.
3. **It tells you, once, without fuss.** A small note appears — *"Paused Chrome to
   keep you fast"* — and fades on its own. You never have to click it away.
4. **It wakes everything back up** the instant the pressure clears. More often than
   not, you won't even notice it happened.

The other creatures in this niche
([earlyoom](https://github.com/rfjakob/earlyoom),
[systemd-oomd](https://www.freedesktop.org/software/systemd/man/systemd-oomd.service.html))
wait until things are dire and then *terminate* an app — your tabs gone, no
warning. rtux's whole strategy is the reversible one: pause, then un-pause. (Full
credit to the prior art in [DESIGN.md](DESIGN.md#prior-art--influences).)

*(There is also **zram** — a clever trick that fits far more into memory by
compressing it, the very thing that keeps phones and Macs smooth at capacity.
rtux's installer offers to switch it on. It is the single biggest improvement, and
Ubuntu, curiously, leaves it off by default.)*

## Bringing it home

The quick-start above covers a modern GNOME desktop. A few notes for everything
else.

The control window needs GTK 4 and **libadwaita ≥ 1.5** (Ubuntu 24.04+, Fedora
39+, current Arch):

```sh
# Ubuntu/Debian
sudo apt install libgtk-4-dev libadwaita-1-dev
# Fedora
sudo dnf install gtk4-devel libadwaita-devel
# Arch
sudo pacman -S gtk4 libadwaita
# Nix
nix-shell -p gtk4 libadwaita
```

On an older box — Ubuntu 22.04 ships libadwaita 1.1 — skip the window and raise
just the guardian, which needs no graphics libraries at all:

```sh
cargo build --release        # daemon only — no GTK / libadwaita
sudo ./install.sh
```

Two small things: `setup-hotkey.sh` binds `Ctrl+Alt+P` and avoids `<Super>`
combos (GNOME grabs those); the top-bar extension only appears after your next
login (GNOME can't load a new extension into the running session).

### Checking it took

```sh
systemctl status rtux.service     # should say "active (running)"
pressured status                  # current pressure + the top consumers
pressured ctl list                # the same census the window shows
```

If the hotkey does nothing, confirm `pressured-hud` is on your `PATH` (the full
build installs it). Watch the guardian work in real time with
`journalctl -u rtux.service -f`.

## Summoning the control window

Press your hotkey and a small window appears: every running app and its memory
appetite, a live trace of pressure over the last minute, and — for any app —
gentle, reversible actions, in order of escalating consequence:

- **Pin** — keep this app fast, always.
- **Cap** — ask it to slim down.
- **Pause** — freeze it (wake it whenever you like).
- **Close** — the only one you can't undo; shut it down.

Everything is labelled in plain words, and every number explains itself if you
rest the pointer on it. The window keeps *itself* in fast memory, so it always
opens instantly — especially when the machine is struggling and you need it most.

*(Prefer the terminal? `pressured ctl list` shows the same census; `pressured
status` gives a quick reading of the pressure.)*

## Returning it to the wild (uninstalling)

rtux leaves no trace it can't undo. One script reverses the whole thing — the
service and binaries, the tray autostart, the hotkey, the top-bar extension, and
the zram config and tuning:

```sh
sudo ./uninstall.sh
```

Or just pause the guardian for now, keeping everything installed:

```sh
sudo systemctl disable --now rtux.service
```

Any memory limits the guardian set reset on the next reboot. (The zram *package*
is left installed — remove it with your package manager if you wish.)

## Field status

The full intervention has been observed in the wild: under genuine memory
pressure, rtux paused a 1.1 GB browser, held the desktop perfectly responsive, and
reported itself — unprompted, and entirely unbothered. What remains on the horizon
— following your gaze to warm up the app you're about to reach for; letting a
frozen window visibly *frost over* in the corner of your eye rather than send a
note — is chronicled in [DESIGN.md](DESIGN.md).

*The human returns to their work. The machine hums along. All is well in the
clearing — for now.*
