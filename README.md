<div align="center">

# gamescope-idle

**Controller-aware idle blanking for Steam Gaming Mode — turn the screen black on idle to protect your OLED, without ever blanking mid-game.**

gamescope-idle watches your keyboard *and* game controllers directly, and when
there's been no input for a while it covers the screen with black to protect an
OLED panel from burn-in. Apps can **inhibit** blanking (so a movie doesn't get
interrupted) and **trigger** it on demand. It's a small Rust daemon that hooks
into the gamescope session and gets out of the way in the KDE desktop.

[![CI](https://github.com/gehhilfe/gamescope-idle/actions/workflows/ci.yml/badge.svg)](https://github.com/gehhilfe/gamescope-idle/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

</div>

## Why this exists

Steam's Gaming Mode runs on [gamescope](https://github.com/ValveSoftware/gamescope),
and gamescope is a hostile environment for ordinary idle tools:

- **No idle protocol.** gamescope exposes none of `ext-idle-notify-v1`,
  `idle-inhibit-unstable-v1`, or `org.kde.kwin.idle`. So `swayidle`, `wlopm`, and
  friends simply do nothing under it.
- **Controllers are invisible to the compositor.** Steam Input grabs your gamepad
  and re-emits it as a virtual pad; the input never reaches gamescope as pointer
  or keyboard events. A compositor idle timer would treat an intense controller
  session as "idle" and blank on you.
- **No blank lever.** gamescope holds the DRM master, so you can't set DPMS from
  outside it, and an external monitor/TV has no backlight to dim.

gamescope-idle solves all three: it reads `/dev/input/event*` **directly** (so
keyboard *and* controller both count as activity), and it blanks by drawing a
fullscreen opaque black surface on the `wlr-layer-shell` **overlay** layer (which
gamescope *does* support). On an OLED, black pixels emit no light — exactly the
burn-in protection you want. Where an HDMI-CEC adapter is present, it can also put
the TV into real standby.

```
 keyboard ─┐                                        ┌─▶ black overlay (wlr-layer-shell)
 gamepad ──┼─▶ evdev ─▶ [ gamescope-idle daemon ] ──┤
 (evdev)   │              state machine             └─▶ CEC standby  (cec-ctl, if present)
           │                    ▲
 logind idle inhibitor ─────────┘  (apps hold one to prevent blanking)
```

## Behaviour

```
ACTIVE ──(no input for idle_timeout, and not inhibited)──▶ DIM ──(dim_warning)──▶ BLACK
   ▲                                                                                │
   └──────────────── any keyboard/controller input, or `wake` ──────────────────────┘
```

- Any keyboard or controller input wakes it instantly.
- Controller stick drift / gyro noise is filtered with a per-axis deadzone, so it
  doesn't keep the screen awake — but D-pad, buttons and real stick pushes do.
- While an **idle inhibitor** is held, it stays awake and never blanks.

### Controllers: in-game vs. the Steam launcher

In a **game**, Steam Input re-emits your controller as a virtual pad on evdev,
which gamescope-idle reads directly (buttons, sticks, D-pad — all count).

In the **Steam launcher / Big Picture UI**, Steam consumes the controller raw
over hidraw and emits no evdev events, so there's nothing on evdev to see. For
known controllers (currently the Valve Steam Controller / Puck) gamescope-idle
reads the hidraw report and detects **motion** — the controller's orientation and
gyro. Steam's raw report has no cleanly-decodable button byte, but motion is the
right proxy: while you navigate you're holding the pad (so it stays awake), and
when you set it down it goes still and the screen blanks. Disable with
`watch_hidraw = false`. Other controllers in the launcher aren't detected yet;
keyboard always is.

## Install

### Arch / CachyOS

```sh
# from a checkout
cargo build --release
sudo install -Dm755 target/release/gamescope-idle /usr/bin/gamescope-idle
sudo install -Dm644 data/gamescope-idle.service /usr/lib/systemd/user/gamescope-idle.service
```

Or build a package with the included [`packaging/PKGBUILD`](packaging/PKGBUILD).

### Enable it

The service is tied to `gamescope-session.target`, so it runs only in Gaming Mode
and stops automatically when you switch to the KDE desktop (which has its own idle
handling).

```sh
systemctl --user enable gamescope-idle.service
# start it now if you're already in Gaming Mode:
systemctl --user start gamescope-idle.service
```

## Configuration

Optional; copy [`data/config.example.toml`](data/config.example.toml) to
`~/.config/gamescope-idle/config.toml`. Defaults:

| Key | Default | Meaning |
|-----|---------|---------|
| `idle_timeout` | `300` | seconds of no input before blanking begins |
| `dim_warning`  | `30`  | seconds dimmed as a warning before full black (`0` = straight to black) |
| `dim_alpha`    | `0.5` | opacity of the dim warning |
| `cec`          | `"auto"` | `auto` / `on` / `off` — HDMI-CEC TV standby |
| `cec_device`   | `"/dev/cec0"` | CEC adapter to use |
| `ignore_devices` | `[]` | extra input event nodes to ignore |

## Preventing and triggering blanking (for apps)

**Prevent** blanking while your app runs — hold a logind idle inhibitor. Either
use the built-in helper:

```sh
gamescope-idle inhibit --why "watching a movie" -- your-app
```

or the standard equivalent:

```sh
systemd-inhibit --what=idle --who=your-app --why="watching a movie" your-app
```

For example, to keep the screen on while [couchcast](https://github.com/gehhilfe/Couchcast)
displays an HDMI capture, wrap its `Exec=` line in a `.desktop` file:

```ini
Exec=gamescope-idle inhibit --why "HDMI capture" -- /path/to/couchcast
```

**Trigger** blanking on demand, and query state:

```sh
gamescope-idle blank     # go black now
gamescope-idle wake      # wake now
gamescope-idle status    # active | dim | black
```

## Debugging — "why won't it blank?"

The daemon logs to the journal (`journalctl --user -u gamescope-idle -f`). At the
default `info` level you see the `dimming` / `blanking` / `awake` transitions. To
find out **which device or app is keeping the screen awake**, enable debug logging:

```sh
systemctl --user edit gamescope-idle
# add:
#   [Service]
#   Environment=RUST_LOG=gamescope_idle=debug
systemctl --user restart gamescope-idle
journalctl --user -u gamescope-idle -f
```

You'll then get lines like:

```
activity from event26 (Microsoft X-Box 360 pad 0): type=KEY code=304 value=1
idle inhibited by gamescope-idle (pid 175803): movie night
```

The first names the input device and event that reset the idle timer (throttled to
one line per device per second); the second names the app holding an idle
inhibitor. Run the daemon by hand with `RUST_LOG=gamescope_idle=debug gamescope-idle daemon`
for the same output on stderr. If a device shows up as unexpected activity, add its
`eventN` node to `ignore_devices` in the config.

## HDMI-CEC (OLED TVs)

With a CEC-capable adapter (e.g. a DisplayPort/HDMI adapter that exposes
`/dev/cec*`), set `cec = "auto"` (the default) and the TV is put into real standby
on blank and woken on input via `cec-ctl` (from `v4l-utils`). With no adapter,
the black overlay alone protects the panel.

## License

Licensed under either of [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at
your option.
