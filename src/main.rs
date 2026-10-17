use std::collections::HashSet;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::Duration;

use cosmic::app::{Core, Task};
use cosmic::applet;
use cosmic::cctk::cosmic_protocols::toplevel_info::v1::client::zcosmic_toplevel_handle_v1;
use cosmic::cctk::cosmic_protocols::toplevel_management::v1::client::zcosmic_toplevel_manager_v1;
use cosmic::cctk::sctk as sctk;
use cosmic::cctk::sctk::reexports::calloop;
use cosmic::cctk::sctk::reexports::calloop_wayland_source::WaylandSource;
use cosmic::cctk::sctk::registry::{ProvidesRegistryState, RegistryState};
use cosmic::cctk::sctk::seat::{SeatHandler, SeatState};
use cosmic::cctk::toplevel_info::{ToplevelInfo, ToplevelInfoHandler, ToplevelInfoState};
use cosmic::cctk::toplevel_management::{ToplevelManagerHandler, ToplevelManagerState};
use cosmic::cctk::wayland_client::{
    Connection, QueueHandle, WEnum,
    delegate_noop,
    globals::registry_queue_init,
    protocol::wl_seat::WlSeat,
};
use cosmic::cctk::wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1;
use cosmic::iced::{self, Subscription};
use cosmic::iced::futures as futures;
use futures::channel::mpsc;
use futures::SinkExt;
use futures::StreamExt;
use tokio::runtime::Handle;

fn socket_path() -> std::io::Result<PathBuf> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set")
    })?;
    let path = PathBuf::from(runtime_dir);
    if !path.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "XDG_RUNTIME_DIR must be absolute",
        ));
    }
    Ok(path.join("show-desktop-cosmic.sock"))
}

fn ipc_listener() -> std::io::Result<UnixListener> {
    let path = socket_path()?;
    match UnixListener::bind(&path) {
        Ok(listener) => Ok(listener),
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            // Only remove a stale socket; never steal a running instance's endpoint.
            match UnixStream::connect(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                    std::fs::remove_file(&path)?;
                    UnixListener::bind(path)
                }
                _ => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> cosmic::iced::Result {
    let args: Vec<String> = std::env::args().collect();

    if args.len() > 1 && args[1] == "--toggle" {
        match socket_path().and_then(|path| {
            let mut stream = UnixStream::connect(path)?;
            stream.set_write_timeout(Some(Duration::from_secs(1)))?;
            stream.write_all(b"toggle")
        }) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("show-desktop-cosmic: failed to send toggle ({e})");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(tracing::level_filters::LevelFilter::WARN.into())
                .from_env_lossy(),
        )
        .init();
    applet::run::<ShowDesktop>(())
}

// ---------------------------------------------------------------------------
// Applet model
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ShowDesktop {
    core: Core,
    showing_desktop: bool,
    hidden_by_us: HashSet<ExtForeignToplevelHandleV1>,
    tx: Option<calloop::channel::Sender<WaylandRequest>>,
    toplevels: Vec<(ExtForeignToplevelHandleV1, ToplevelInfo)>,
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Message {
    Toggle,
    Wayland(WaylandUpdate),
}

#[derive(Debug, Clone)]
enum WaylandUpdate {
    Init(calloop::channel::Sender<WaylandRequest>),
    Finished,
    Toplevel(ToplevelUpdate),
}

#[derive(Debug, Clone)]
enum ToplevelUpdate {
    Add(ExtForeignToplevelHandleV1, Box<ToplevelInfo>),
    Update(ExtForeignToplevelHandleV1, Box<ToplevelInfo>),
    Remove(ExtForeignToplevelHandleV1),
}

#[derive(Debug, Clone)]
enum WaylandRequest {
    Minimize(ExtForeignToplevelHandleV1),
    Unminimize(ExtForeignToplevelHandleV1),
}

// ---------------------------------------------------------------------------
// Wayland subscription
// ---------------------------------------------------------------------------

fn wayland_subscription() -> iced::Subscription<WaylandUpdate> {
    Subscription::run_with(std::any::TypeId::of::<WaylandUpdate>(), |_| {
        iced::stream::channel(
            1,
            move |mut output: futures::channel::mpsc::Sender<WaylandUpdate>| async move {
                let (calloop_tx, calloop_rx) = calloop::channel::channel();
                let runtime = Handle::current();
                let _ = std::thread::spawn(move || {
                    runtime.block_on(async move {
                        let _ = output.send(WaylandUpdate::Init(calloop_tx)).await;
                        wayland_handler(output.clone(), calloop_rx);
                        let _ = output.send(WaylandUpdate::Finished).await;
                    });
                });
                futures::future::pending().await
            },
        )
    })
}

// ---------------------------------------------------------------------------
// IPC subscription (Unix socket for --toggle)
// ---------------------------------------------------------------------------

fn ipc_subscription() -> iced::Subscription<Message> {
    Subscription::run_with(std::any::TypeId::of::<Message>(), |_| {
        iced::stream::channel(
            1,
            move |mut output: futures::channel::mpsc::Sender<Message>| async move {
                let listener = match ipc_listener() {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::error!("Failed to bind IPC socket: {e}");
                        return;
                    }
                };

                let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();

                std::thread::spawn(move || {
                    for stream in listener.incoming() {
                        match stream {
                            Ok(mut stream) => {
                                if stream
                                    .set_read_timeout(Some(Duration::from_secs(1)))
                                    .is_err()
                                {
                                    continue;
                                }
                                let mut buf = [0; 6];
                                if stream.read_exact(&mut buf).is_ok()
                                    && &buf == b"toggle"
                                    && tx.unbounded_send(()).is_err()
                                {
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::error!("IPC accept error: {e}");
                                break;
                            }
                        }
                    }
                });

                while rx.next().await.is_some() {
                    if output.send(Message::Toggle).await.is_err() {
                        break;
                    }
                }
            },
        )
    })
}

// ---------------------------------------------------------------------------
// Wayland handler
// ---------------------------------------------------------------------------

#[allow(dead_code)]
struct AppData {
    exit: bool,
    tx: mpsc::Sender<WaylandUpdate>,
    conn: Connection,
    registry_state: RegistryState,
    toplevel_info_state: ToplevelInfoState,
    toplevel_manager_state: ToplevelManagerState,
    seat_state: SeatState,
    seat: Option<WlSeat>,
    queue_handle: QueueHandle<Self>,
}

impl ProvidesRegistryState for AppData {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    sctk::registry_handlers!();
}

impl SeatHandler for AppData {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: WlSeat) {
        self.seat = Some(seat);
    }
    fn new_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: WlSeat,
        _: cosmic::cctk::sctk::seat::Capability,
    ) {
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: WlSeat,
        _: cosmic::cctk::sctk::seat::Capability,
    ) {
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}
}

impl ToplevelInfoHandler for AppData {
    fn toplevel_info_state(&mut self) -> &mut ToplevelInfoState {
        &mut self.toplevel_info_state
    }

    fn new_toplevel(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        toplevel: &ExtForeignToplevelHandleV1,
    ) {
        if let Some(info) = self.toplevel_info_state.info(toplevel) {
            let _ = futures::executor::block_on(self.tx.send(WaylandUpdate::Toplevel(
                ToplevelUpdate::Add(toplevel.clone(), Box::new(info.clone())),
            )));
        }
    }

    fn update_toplevel(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        toplevel: &ExtForeignToplevelHandleV1,
    ) {
        if let Some(info) = self.toplevel_info_state.info(toplevel) {
            let _ = futures::executor::block_on(self.tx.send(WaylandUpdate::Toplevel(
                ToplevelUpdate::Update(toplevel.clone(), Box::new(info.clone())),
            )));
        }
    }

    fn toplevel_closed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        toplevel: &ExtForeignToplevelHandleV1,
    ) {
        let _ = futures::executor::block_on(self.tx.send(WaylandUpdate::Toplevel(
            ToplevelUpdate::Remove(toplevel.clone()),
        )));
    }
}

impl ToplevelManagerHandler for AppData {
    fn toplevel_manager_state(&mut self) -> &mut ToplevelManagerState {
        &mut self.toplevel_manager_state
    }

    fn capabilities(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _caps: Vec<WEnum<zcosmic_toplevel_manager_v1::ZcosmicToplelevelManagementCapabilitiesV1>>,
    ) {
    }
}

impl AppData {
    fn cosmic_toplevel(
        &self,
        handle: &ExtForeignToplevelHandleV1,
    ) -> Option<
        cosmic::cctk::cosmic_protocols::toplevel_info::v1::client::zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1,
    >{
        self.toplevel_info_state
            .info(handle)?
            .cosmic_toplevel
            .clone()
    }
}

sctk::delegate_seat!(AppData);
sctk::delegate_registry!(AppData);
cctk::delegate_toplevel_info!(AppData);
cctk::delegate_toplevel_manager!(AppData);
delegate_noop!(AppData: ignore cosmic::cctk::wayland_client::protocol::wl_buffer::WlBuffer);

fn wayland_handler(tx: mpsc::Sender<WaylandUpdate>, rx: calloop::channel::Channel<WaylandRequest>) {
    let conn = Connection::connect_to_env().expect("Failed to connect to Wayland compositor");
    let (globals, event_queue) = registry_queue_init(&conn).expect("Failed to init registry");

    let mut event_loop: calloop::EventLoop<AppData> =
        calloop::EventLoop::try_new().expect("Failed to create event loop");
    let qh = event_queue.handle();
    let wayland_source = WaylandSource::new(conn.clone(), event_queue);
    let handle = event_loop.handle();

    wayland_source
        .insert(handle.clone())
        .expect("Failed to insert wayland source");

    handle
        .insert_source(rx, |event, _metadata, state| match event {
            calloop::channel::Event::Msg(req) => match req {
                WaylandRequest::Minimize(h) => {
                    if let Some(cosmic) = state.cosmic_toplevel(&h) {
                        state.toplevel_manager_state.manager.set_minimized(&cosmic);
                    }
                }
                WaylandRequest::Unminimize(h) => {
                    if let Some(cosmic) = state.cosmic_toplevel(&h) {
                        state
                            .toplevel_manager_state
                            .manager
                            .unset_minimized(&cosmic);
                    }
                }
            },
            calloop::channel::Event::Closed => {
                state.exit = true;
            }
        })
        .expect("Failed to insert channel source");

    let registry_state = RegistryState::new(&globals);
    let toplevel_info_state = ToplevelInfoState::new(&registry_state, &qh);
    let toplevel_manager_state = ToplevelManagerState::new(&registry_state, &qh);
    let seat_state = SeatState::new(&globals, &qh);
    let first_seat = seat_state.seats().next();

    let mut app_data = AppData {
        exit: false,
        tx,
        conn,
        registry_state,
        toplevel_info_state,
        toplevel_manager_state,
        seat_state,
        seat: first_seat,
        queue_handle: qh,
    };

    loop {
        if app_data.exit {
            break;
        }
        if event_loop.dispatch(None, &mut app_data).is_err() {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// cosmic::Application
// ---------------------------------------------------------------------------

impl cosmic::Application for ShowDesktop {
    type Message = Message;
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = ();
    const APP_ID: &'static str = "com.system76.CosmicAppletShowDesktop";

    fn init(core: Core, _flags: ()) -> (Self, Task<Message>) {
        (
            Self {
                core,
                ..Default::default()
            },
            Task::none(),
        )
    }

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Toggle => {
                let Some(tx) = &self.tx else {
                    return Task::none();
                };
                if self.showing_desktop {
                    self.hidden_by_us
                        .retain(|h| tx.send(WaylandRequest::Unminimize(h.clone())).is_err());
                    self.showing_desktop = !self.hidden_by_us.is_empty();
                } else {
                    let to_minimize: Vec<_> = self
                        .toplevels
                        .iter()
                        .filter(|(_, info)| {
                            info.cosmic_toplevel.is_some()
                                && !info
                                    .state
                                    .contains(&zcosmic_toplevel_handle_v1::State::Minimized)
                        })
                        .map(|(h, _)| h.clone())
                        .collect();

                    if !to_minimize.is_empty() {
                        for h in to_minimize {
                            if tx.send(WaylandRequest::Minimize(h.clone())).is_ok() {
                                self.hidden_by_us.insert(h);
                            }
                        }
                        self.showing_desktop = !self.hidden_by_us.is_empty();
                    }
                }
            }
            Message::Wayland(update) => match update {
                WaylandUpdate::Init(tx) => {
                    self.tx = Some(tx);
                }
                WaylandUpdate::Finished => {
                    tracing::error!("Wayland handler exited");
                    self.tx = None;
                    self.toplevels.clear();
                    self.hidden_by_us.clear();
                    self.showing_desktop = false;
                }
                WaylandUpdate::Toplevel(t) => match t {
                    ToplevelUpdate::Add(handle, info) => {
                        if !self.toplevels.iter().any(|(h, _)| *h == handle) {
                            self.toplevels.push((handle, *info));
                        }
                    }
                    ToplevelUpdate::Update(handle, info) => {
                        if let Some((_, existing)) =
                            self.toplevels.iter_mut().find(|(h, _)| *h == handle)
                        {
                            *existing = *info;
                        }
                    }
                    ToplevelUpdate::Remove(handle) => {
                        self.hidden_by_us.remove(&handle);
                        self.showing_desktop = !self.hidden_by_us.is_empty();
                        self.toplevels.retain(|(h, _)| *h != handle);
                    }
                },
            },
        }
        Task::none()
    }

    fn view(&self) -> cosmic::Element<'_, Message> {
        let icon = if self.showing_desktop {
            "video-display-symbolic"
        } else {
            "user-desktop-symbolic"
        };
        self.core
            .applet
            .icon_button(icon)
            .on_press_down(Message::Toggle)
            .into()
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch(vec![
            wayland_subscription().map(Message::Wayland),
            ipc_subscription(),
        ])
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(applet::style())
    }
}
