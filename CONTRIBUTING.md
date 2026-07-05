# Contributing to gamescope-idle

Thanks for your interest in improving gamescope-idle!

## Ground rules

- Be respectful — see [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md).
- Discuss non-trivial changes in an issue before opening a large PR.
- By contributing, you agree that your contributions are dual-licensed under
  [MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE), matching the project.

## Development

```sh
cargo build
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

The daemon only does something useful inside a running gamescope session. For a
fast feedback loop, point it at a config with short timers and run it by hand:

```sh
cat > /tmp/gi.toml <<'EOF'
idle_timeout = 5
dim_warning = 2
cec = "off"
EOF
WAYLAND_DISPLAY=gamescope-0 cargo run -- daemon --config /tmp/gi.toml
```

There is a hidden `overlay-test` subcommand to check the black overlay composites
on your compositor without running the whole state machine:

```sh
WAYLAND_DISPLAY=gamescope-0 cargo run -- overlay-test --alpha 1.0 --seconds 3
```

## Module map

| File | Responsibility |
|------|----------------|
| `src/input.rs`   | evdev enumeration, hotplug, keyboard/controller activity (with ABS deadzone) |
| `src/overlay.rs` | `wlr-layer-shell` black/dim overlay on its own thread |
| `src/inhibit.rs` | logind `BlockInhibited` watch + the `inhibit` subcommand |
| `src/cec.rs`     | `/dev/cec*` detection + `cec-ctl` standby/wake |
| `src/control.rs` | unix-socket control protocol (`blank`/`wake`/`status`) |
| `src/daemon.rs`  | the state machine tying it all together |
| `src/config.rs`  | TOML config + defaults |
