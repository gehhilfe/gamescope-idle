//! Fullscreen black/dim overlay via the `wlr-layer-shell` protocol
//! (`zwlr_layer_shell_v1`, which gamescope exposes).
//!
//! This is the display-agnostic way to black an OLED under gamescope: gamescope
//! holds the DRM master so we cannot set DPMS, and external panels/TVs have no
//! backlight to dim. An opaque black surface on the *overlay* layer turns every
//! pixel off — exactly what protects an OLED from burn-in.
//!
//! Wayland state is confined to a dedicated thread with its own `calloop` loop;
//! the daemon drives it with [`OverlayCmd`] messages over a `calloop` channel.

use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use calloop::EventLoop;
use calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
    Connection, QueueHandle,
};

/// Commands the daemon sends to the overlay thread.
#[derive(Debug, Clone, Copy)]
pub enum OverlayCmd {
    /// Show (or update) the overlay at the given alpha (0.0 transparent .. 1.0 black).
    Show { alpha: f64 },
    /// Remove the overlay entirely.
    Hide,
    /// Tear down the thread.
    Quit,
}

/// Handle to the overlay thread. Cloneable senders are cheap.
#[derive(Clone)]
pub struct OverlayHandle {
    tx: calloop::channel::Sender<OverlayCmd>,
}

impl OverlayHandle {
    pub fn show(&self, alpha: f64) {
        let _ = self.tx.send(OverlayCmd::Show { alpha });
    }
    pub fn hide(&self) {
        let _ = self.tx.send(OverlayCmd::Hide);
    }
    pub fn quit(&self) {
        let _ = self.tx.send(OverlayCmd::Quit);
    }
}

/// Start the overlay thread and return a handle to control it.
pub fn spawn() -> Result<OverlayHandle> {
    let (tx, rx) = calloop::channel::channel::<OverlayCmd>();
    // Confirm we can reach the compositor before returning, so callers get an early error.
    let conn = Connection::connect_to_env()
        .context("connecting to the Wayland compositor (is WAYLAND_DISPLAY=gamescope-0 set?)")?;

    thread::Builder::new()
        .name("gi-overlay".into())
        .spawn(move || {
            if let Err(e) = run(conn, rx) {
                tracing::error!("overlay thread exited: {e:#}");
            }
        })
        .context("spawning overlay thread")?;

    Ok(OverlayHandle { tx })
}

fn run(conn: Connection, rx: calloop::channel::Channel<OverlayCmd>) -> Result<()> {
    let (globals, event_queue) = registry_queue_init(&conn).context("registry init")?;
    let qh: QueueHandle<Overlay> = event_queue.handle();

    let mut event_loop: EventLoop<Overlay> =
        EventLoop::try_new().context("creating calloop event loop")?;
    let handle = event_loop.handle();

    WaylandSource::new(conn, event_queue)
        .insert(handle.clone())
        .map_err(|e| anyhow::anyhow!("inserting wayland source: {e}"))?;
    handle
        .insert_source(rx, |event, _, state: &mut Overlay| {
            if let calloop::channel::Event::Msg(cmd) = event {
                state.on_cmd(cmd);
            }
        })
        .map_err(|e| anyhow::anyhow!("inserting command channel: {e}"))?;

    let compositor = CompositorState::bind(&globals, &qh).context("wl_compositor missing")?;
    let layer_shell = LayerShell::bind(&globals, &qh).context("wlr-layer-shell missing")?;
    let shm = Shm::bind(&globals, &qh).context("wl_shm missing")?;

    let mut state = Overlay {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,
        compositor,
        layer_shell,
        qh: qh.clone(),
        pool: None,
        layer: None,
        input_region: None,
        size: (0, 0),
        output_size: None,
        alpha: 0.0,
        exit: false,
    };

    while !state.exit {
        event_loop
            .dispatch(Some(Duration::from_millis(500)), &mut state)
            .context("event loop dispatch")?;
    }
    Ok(())
}

struct Overlay {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    compositor: CompositorState,
    layer_shell: LayerShell,
    qh: QueueHandle<Overlay>,
    pool: Option<SlotPool>,
    layer: Option<LayerSurface>,
    input_region: Option<Region>,
    size: (u32, u32),
    output_size: Option<(u32, u32)>,
    alpha: f64,
    exit: bool,
}

impl Overlay {
    fn on_cmd(&mut self, cmd: OverlayCmd) {
        match cmd {
            // "Hide" is just a fully-transparent redraw — we deliberately never
            // destroy the surface while the daemon runs. Destroying it and then
            // continuing to dispatch makes gamescope error the connection (an
            // event arrives for the dead surface → broken pipe). Keeping one
            // persistent, click-through surface and only changing its alpha is
            // robust and avoids map/unmap churn.
            OverlayCmd::Show { alpha } => self.set_alpha(alpha),
            OverlayCmd::Hide => self.set_alpha(0.0),
            OverlayCmd::Quit => {
                self.layer = None; // safe here: we stop dispatching immediately after
                self.exit = true;
            }
        }
    }

    fn set_alpha(&mut self, alpha: f64) {
        self.alpha = alpha.clamp(0.0, 1.0);
        if self.layer.is_none() {
            self.create_layer();
        } else {
            self.draw();
        }
    }

    fn create_layer(&mut self) {
        let surface = self.compositor.create_surface(&self.qh);

        // Empty input region → the overlay never steals pointer/touch input;
        // activity is detected from evdev, and the layer must not block Steam.
        if let Ok(region) = Region::new(&self.compositor) {
            surface.set_input_region(Some(region.wl_region()));
            self.input_region = Some(region);
        }

        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            surface,
            Layer::Overlay,
            Some("gamescope-idle"),
            None,
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(0, 0); // 0,0 + all anchors = fill the output
        layer.commit();
        self.layer = Some(layer);
        // Actual paint happens on the first `configure`.
    }

    fn effective_size(&self, configured: (u32, u32)) -> (u32, u32) {
        if configured.0 != 0 && configured.1 != 0 {
            configured
        } else {
            self.output_size.unwrap_or((1920, 1080))
        }
    }

    fn draw(&mut self) {
        let (w, h) = self.size;
        if w == 0 || h == 0 {
            return;
        }
        let Some(layer) = self.layer.as_ref() else {
            return;
        };
        let stride = w as i32 * 4;
        let len = (stride * h as i32) as usize;

        let pool = match self.pool.as_mut() {
            Some(p) => p,
            None => match SlotPool::new(len, &self.shm) {
                Ok(p) => self.pool.insert(p),
                Err(e) => {
                    tracing::error!("shm pool: {e}");
                    return;
                }
            },
        };

        let (buffer, canvas) =
            match pool.create_buffer(w as i32, h as i32, stride, wl_shm::Format::Argb8888) {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::error!("create_buffer: {e}");
                    return;
                }
            };

        // Premultiplied ARGB8888, little-endian byte order is [B, G, R, A].
        let a = (self.alpha * 255.0).round() as u8;
        for px in canvas.chunks_exact_mut(4) {
            px[0] = 0;
            px[1] = 0;
            px[2] = 0;
            px[3] = a;
        }

        let surface = layer.wl_surface();
        if let Err(e) = buffer.attach_to(surface) {
            tracing::error!("attach: {e}");
            return;
        }
        surface.damage_buffer(0, 0, w as i32, h as i32);
        surface.commit();
        tracing::debug!("drew {}x{} overlay at alpha={}", w, h, self.alpha);
    }
}

impl LayerShellHandler for Overlay {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.layer = None;
        self.size = (0, 0);
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        self.size = self.effective_size(configure.new_size);
        tracing::debug!(
            "layer configured: requested={:?} using={:?} alpha={}",
            configure.new_size,
            self.size,
            self.alpha
        );
        self.draw();
    }
}

impl CompositorHandler for Overlay {
    fn scale_factor_changed(
        &mut self,
        _c: &Connection,
        _q: &QueueHandle<Self>,
        _s: &wl_surface::WlSurface,
        _new: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _c: &Connection,
        _q: &QueueHandle<Self>,
        _s: &wl_surface::WlSurface,
        _t: wl_output::Transform,
    ) {
    }
    fn frame(
        &mut self,
        _c: &Connection,
        _q: &QueueHandle<Self>,
        _s: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }
    fn surface_enter(
        &mut self,
        _c: &Connection,
        _q: &QueueHandle<Self>,
        _s: &wl_surface::WlSurface,
        _o: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _c: &Connection,
        _q: &QueueHandle<Self>,
        _s: &wl_surface::WlSurface,
        _o: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for Overlay {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _c: &Connection, _q: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.remember_output(&output);
    }
    fn update_output(
        &mut self,
        _c: &Connection,
        _q: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.remember_output(&output);
    }
    fn output_destroyed(
        &mut self,
        _c: &Connection,
        _q: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl Overlay {
    fn remember_output(&mut self, output: &wl_output::WlOutput) {
        if let Some(info) = self.output_state.info(output) {
            if let Some((w, h)) = info
                .logical_size
                .map(|(w, h)| (w as u32, h as u32))
                .or_else(|| {
                    info.modes
                        .iter()
                        .find(|m| m.current)
                        .map(|m| (m.dimensions.0 as u32, m.dimensions.1 as u32))
                })
            {
                self.output_size = Some((w, h));
            }
        }
    }
}

impl ShmHandler for Overlay {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for Overlay {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

delegate_compositor!(Overlay);
delegate_output!(Overlay);
delegate_shm!(Overlay);
delegate_layer!(Overlay);
delegate_registry!(Overlay);
