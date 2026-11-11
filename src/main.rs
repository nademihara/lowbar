//! lowbar — a tiny Wayland status bar: workspaces, clock, battery.
//!
//! Talks to the compositor directly, no widget toolkit involved:
//!
//!   * `zwlr_layer_shell_v1`      — the bar surface itself
//!   * `ext_workspace_manager_v1` — workspace names + which one is active
//!   * `wl_shm`                   — the pixels we paint into
//!
//! The clock and battery are read from the filesystem once a second. If
//! nothing changed since the last frame, nothing is redrawn.

mod render;
mod status;

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::AsFd;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{backend::ObjectId, Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, ZwlrLayerSurfaceV1},
};

use crate::render::{draw_bar, Renderer, HEIGHT};
use crate::status::Status;

/// Namespace we register the layer surface under (used by compositor rules).
const NAMESPACE: &str = "lowbar";

/// What we know about one workspace handle from the compositor.
#[derive(Default)]
struct Workspace {
    name: String,
    active: bool,
    hidden: bool,
}

/// All the mutable state of the bar.
struct App {
    // -- bar surface --------------------------------------------------------
    surface: wl_surface::WlSurface,
    layer_surface: ZwlrLayerSurfaceV1,
    configured: bool,
    closed: bool,
    /// Output scale (integer HiDPI factor).
    scale: i32,
    width: u32,
    height: u32,
    /// Set when a redraw is needed regardless of content (resize, rescale).
    need_redraw: bool,

    // -- shared memory double buffer -----------------------------------------
    conn: Connection,
    qh: QueueHandle<App>,
    shm: wl_shm::WlShm,
    shm_file: Option<std::fs::File>,
    buffers: [Option<wl_buffer::WlBuffer>; 2],
    buffer_offset: [usize; 2],
    buffer_free: [bool; 2],
    buffer_size: (usize, usize),
    pixels: Vec<u8>,

    // -- content --------------------------------------------------------------
    workspaces: HashMap<ObjectId, Workspace>,
    status: Option<Status>,
    renderer: Renderer,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("lowbar: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let renderer = Renderer::load()?;

    let conn = Connection::connect_to_env().map_err(|e| format!("connect: {e}"))?;
    let (globals, mut queue) =
        registry_queue_init::<App>(&conn).map_err(|e| format!("globals: {e}"))?;
    let qh = queue.handle();

    // single-instance globals (None → optional feature)
    let shm: wl_shm::WlShm = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("wl_shm: {e}"))?;
    let compositor: wl_compositor::WlCompositor = globals
        .bind(&qh, 1..=6, ())
        .map_err(|e| format!("wl_compositor: {e}"))?;
    let layer_shell: ZwlrLayerShellV1 = globals
        .bind(&qh, 1..=4, ())
        .map_err(|e| format!("layer_shell: {e}"))?;
    // workspace support is optional — the bar runs without it
    let workspace_manager: Option<ExtWorkspaceManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
    // the bar shows on the first output (single-monitor assumption)
    let output: Option<wl_output::WlOutput> = globals.bind(&qh, 1..=4, ()).ok();

    // create the layer surface before any buffer is attached
    let surface = compositor.create_surface(&qh, ());
    let layer_surface = layer_shell.get_layer_surface(
        &surface,
        output.as_ref(),
        Layer::Top,
        NAMESPACE.to_string(),
        &qh,
        (),
    );
    layer_surface.set_anchor(Anchor::Top | Anchor::Left | Anchor::Right);
    layer_surface.set_exclusive_zone(HEIGHT as i32); // keep windows off the bar
    layer_surface.set_size(0, HEIGHT); // width 0 = "fill the anchors"
    surface.commit(); // must commit once, buffer-less, to get a configure

    let mut app = App {
        surface,
        layer_surface,
        configured: false,
        closed: false,
        scale: 1,
        width: 0,
        height: 0,
        need_redraw: false,
        conn: conn.clone(),
        qh: qh.clone(),
        shm,
        shm_file: None,
        buffers: [None, None],
        buffer_offset: [0, 0],
        buffer_free: [false, false],
        buffer_size: (0, 0),
        pixels: Vec::new(),
        workspaces: HashMap::new(),
        status: None,
        renderer,
    };
    let _ = workspace_manager; // kept alive by the connection; events flow either way

    loop {
        queue
            .roundtrip(&mut app)
            .map_err(|e| format!("event loop: {e}"))?;
        if app.closed {
            break;
        }
        app.tick();
        sleep_until_next_second();
    }

    // politely take the bar down before disconnecting
    app.layer_surface.destroy();
    let _ = app.conn.flush();
    Ok(())
}

/// Sleep until the next whole second so the clock is always fresh.
fn sleep_until_next_second() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let target = Duration::from_secs(now.as_secs() + 1);
    if target > now {
        sleep(target - now);
    }
}

impl App {
    /// Called once per second: gather status, redraw if anything changed.
    fn tick(&mut self) {
        let mut workspaces: Vec<(String, bool)> = self
            .workspaces
            .values()
            .filter(|w| !w.hidden)
            .map(|w| (w.name.clone(), w.active))
            .collect();
        // numeric names ("1".."10") sort naturally, others alphabetically
        workspaces.sort_by(|a, b| match (a.0.parse::<u32>(), b.0.parse::<u32>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            _ => a.0.cmp(&b.0),
        });

        let status = Status::build(workspaces);
        let changed = self.status.as_ref() != Some(&status);
        self.status = Some(status);

        if (changed || self.need_redraw) && self.configured {
            self.draw();
        }
    }

    /// Render the current status and hand it to the compositor.
    fn draw(&mut self) {
        self.need_redraw = false;
        let scale = self.scale.max(1) as usize;
        let bw = self.width as usize * scale;
        let bh = self.height as usize * scale;
        if bw == 0 || bh == 0 {
            return;
        }
        if self.buffer_size != (bw, bh) {
            self.create_buffers(bw, bh);
            if self.closed {
                return;
            }
        }
        // paint into our own memory
        self.pixels.resize(bw * bh * 4, 0);
        let status = self.status.as_ref().expect("tick ran before draw");
        draw_bar(&mut self.pixels, bw, bh, scale, &self.renderer, status);

        // pick a buffer the compositor has released; if both are still in
        // flight we simply skip this frame and try again next second
        let Some(idx) = (0..2).find(|&i| self.buffer_free[i]) else {
            return;
        };

        // copy the pixels into shared memory, behind the chosen buffer
        let Some(file) = self.shm_file.as_mut() else {
            return;
        };
        if let Err(e) = file
            .seek(SeekFrom::Start(self.buffer_offset[idx] as u64))
            .and_then(|_| file.write_all(&self.pixels))
        {
            eprintln!("lowbar: shm write failed: {e}");
            self.closed = true;
            return;
        }

        let buffer = self.buffers[idx].as_ref().expect("buffers created above");
        self.surface.set_buffer_scale(scale as i32);
        self.surface.attach(Some(buffer), 0, 0);
        self.surface.damage_buffer(0, 0, bw as i32, bh as i32);
        self.surface.commit();
        self.buffer_free[idx] = false;
        // roundtrip is a second away — flush now so the frame shows instantly
        if let Err(e) = self.conn.flush() {
            eprintln!("lowbar: flush failed: {e}");
            self.closed = true;
        }
    }

    /// (Re)create the shm pool and the two wl_buffers for a new size.
    fn create_buffers(&mut self, bw: usize, bh: usize) {
        let stride = bw * 4;
        let frame = stride * bh;

        // a plain file in /dev/shm is all a wl_shm_pool needs; it is
        // unlinked right away so nothing lingers even if we crash
        let path = format!("/dev/shm/{NAMESPACE}-{}", std::process::id());
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!("lowbar: {path}: {e}");
                self.closed = true;
                return;
            }
        };
        if let Err(e) = file.set_len((frame * 2) as u64) {
            eprintln!("lowbar: {path}: {e}");
            self.closed = true;
            return;
        }

        let pool = self
            .shm
            .create_pool(file.as_fd(), (frame * 2) as i32, &self.qh, ());
        let qh = self.qh.clone();
        let mk = |offset: i32, idx: usize| {
            pool.create_buffer(
                offset,
                bw as i32,
                bh as i32,
                stride as i32,
                wl_shm::Format::Argb8888,
                &qh,
                idx,
            )
        };

        // the old pool's server-side objects are simply abandoned; this
        // only ever happens on output resize, which is rare
        self.buffers = [Some(mk(0, 0)), Some(mk(frame as i32, 1))];
        self.buffer_offset = [0, frame];
        self.buffer_free = [true, true];
        self.buffer_size = (bw, bh);

        let _ = std::fs::remove_file(&path); // fd stays valid through `file`
        self.shm_file = Some(file);
    }
}

// ---------------------------------------------------------------------------
// protocol event plumbing
// ---------------------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for App {
    fn event(
        _state: &mut App,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qhandle: &QueueHandle<App>,
    ) {
        // globals appearing/disappearing at runtime (hotplug) are ignored
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for App {
    fn event(
        state: &mut App,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<App>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                // the first configure gives us the real size of the bar
                layer.ack_configure(serial);
                state.width = width.max(1);
                state.height = height.max(1);
                state.configured = true;
                state.need_redraw = true;
            }
            zwlr_layer_surface_v1::Event::Closed => state.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_shm::WlShm, ()> for App {
    fn event(
        _state: &mut App,
        _proxy: &wl_shm::WlShm,
        _event: wl_shm::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<App>,
    ) {
        // wl_shm has no events
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for App {
    fn event(
        _state: &mut App,
        _proxy: &wl_compositor::WlCompositor,
        _event: wl_compositor::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<App>,
    ) {
        // wl_compositor has no events
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for App {
    fn event(
        _state: &mut App,
        _proxy: &wl_surface::WlSurface,
        _event: wl_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<App>,
    ) {
        // enter/leave/frame callbacks are not used
    }
}

impl Dispatch<ZwlrLayerShellV1, ()> for App {
    fn event(
        _state: &mut App,
        _proxy: &ZwlrLayerShellV1,
        _event: zwlr_layer_shell_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<App>,
    ) {
        // zwlr_layer_shell_v1 has no events
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for App {
    fn event(
        _state: &mut App,
        _proxy: &wl_shm_pool::WlShmPool,
        _event: wl_shm_pool::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<App>,
    ) {
        // wl_shm_pool has no events
    }
}

impl Dispatch<wl_buffer::WlBuffer, usize> for App {
    fn event(
        state: &mut App,
        _proxy: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        data: &usize,
        _conn: &Connection,
        _qhandle: &QueueHandle<App>,
    ) {
        if matches!(event, wl_buffer::Event::Release) {
            state.buffer_free[*data] = true;
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for App {
    fn event(
        state: &mut App,
        _proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<App>,
    ) {
        if let wl_output::Event::Scale { factor } = event {
            if factor > 0 && factor != state.scale {
                state.scale = factor;
                state.need_redraw = true;
            }
        }
        // geometry / mode / name — not needed
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for App {
    fn event(
        state: &mut App,
        _proxy: &ExtWorkspaceManagerV1,
        event: ext_workspace_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<App>,
    ) {
        // workspace groups are not tracked: we show every workspace the
        // compositor reports (right thing on single-monitor setups)
        if let ext_workspace_manager_v1::Event::Workspace { workspace } = event {
            state
                .workspaces
                .insert(workspace.id(), Workspace::default());
        }
    }

    wayland_client::event_created_child!(App, ExtWorkspaceManagerV1, [
        ext_workspace_manager_v1::EVT_WORKSPACE_OPCODE => (ExtWorkspaceHandleV1, ()),
        ext_workspace_manager_v1::EVT_WORKSPACE_GROUP_OPCODE => (ExtWorkspaceGroupHandleV1, ()),
    ]);
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for App {
    fn event(
        _state: &mut App,
        _proxy: &ExtWorkspaceGroupHandleV1,
        _event: ext_workspace_group_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<App>,
    ) {
        // ignored — see note in the manager impl above
    }
}

impl Dispatch<ExtWorkspaceHandleV1, ()> for App {
    fn event(
        state: &mut App,
        handle: &ExtWorkspaceHandleV1,
        event: ext_workspace_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<App>,
    ) {
        use ext_workspace_handle_v1::Event;
        match event {
            Event::Name { name } => {
                if let Some(w) = state.workspaces.get_mut(&handle.id()) {
                    w.name = name;
                }
            }
            Event::State { state: flags } => {
                let (active, hidden) = match flags {
                    WEnum::Value(f) => (
                        f.contains(ext_workspace_handle_v1::State::Active),
                        f.contains(ext_workspace_handle_v1::State::Hidden),
                    ),
                    WEnum::Unknown(bits) => ((bits & 1) != 0, (bits & 4) != 0),
                };
                if let Some(w) = state.workspaces.get_mut(&handle.id()) {
                    w.active = active;
                    w.hidden = hidden;
                }
            }
            Event::Removed => {
                state.workspaces.remove(&handle.id());
            }
            _ => {} // id / coordinates / capabilities
        }
    }
}
