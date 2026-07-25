use std::collections::HashSet;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

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

const SOCKET_PATH: &str = "/tmp/show-desktop-cosmic.sock";

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> cosmic::iced::Result {
    let args: Vec<String> = std::env::args().collect();

    if args.len() > 1 && args[1] == "--toggle" {
        match UnixStream::connect(SOCKET_PATH) {
            Ok(mut stream) => {
                let _ = stream.write_all(b"toggle");
            }
            Err(e) => {
                eprintln!("show-desktop-cosmic: no instance running ({e})");
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

struct ShowDesktop {
    core: Core,
    showing_desktop: bool,
    hidden_by_us: HashSet<ExtForeignToplevelHandleV1>,
    tx: Option<calloop::channel::Sender<WaylandRequest>>,
    toplevels: Vec<(ExtForeignToplevelHandleV1, ToplevelInfo)>,
}

impl Default for ShowDesktop {
    fn default() -> Self {
        Self {
            core: Core::default(),
            showing_desktop: false,
            hidden_by_us: HashSet::new(),
            tx: None,
            toplevels: Vec::new(),
        }
    }
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
    Add(ExtForeignToplevelHandleV1, ToplevelInfo),
    Update(ExtForeignToplevelHandleV1, ToplevelInfo),
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
                let _ = std::fs::remove_file(SOCKET_PATH);

                let listener = match std::os::unix::net::UnixListener::bind(SOCKET_PATH) {
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
                                let mut buf = [0; 6];
                                if stream.read_exact(&mut buf).is_ok() && &buf == b"toggle" {
                                    let _ = tx.unbounded_send(());
                                }
                            }
                            Err(e) => {
                                tracing::error!("IPC accept error: {e}");
                                break;
                            }
                        }
                    }
                });

                while let Some(_) = rx.next().await {
                    let _ = output.send(Message::Toggle).await;
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
                ToplevelUpdate::Add(toplevel.clone(), info.clone()),
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
                ToplevelUpdate::Update(toplevel.clone(), info.clone()),
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
    > {
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

fn wayland_handler(
    tx: mpsc::Sender<WaylandUpdate>,
    rx: calloop::channel::Channel<WaylandRequest>,
) {
    let conn = Connection::connect_to_env().expect("Failed to connect to Wayland compositor");
    let (globals, event_queue) =
        registry_queue_init(&conn).expect("Failed to init registry");

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
                    let cosmic = state.cosmic_toplevel(&h);
                    let seat = state.seat.as_ref().cloned();
                    if let (Some(c), Some(s)) = (cosmic, seat) {
                        state.toplevel_manager_state.manager.activate(&c, &s);
                        state.toplevel_manager_state.manager.unset_minimized(&c);
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
        (Self {
            core,
            ..Default::default()
        }, Task::none())
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
                if self.showing_desktop {
                    let handles: Vec<_> = self.hidden_by_us.drain().collect();
                    if let Some(tx) = &self.tx {
                        for h in handles {
                            let _ = tx.send(WaylandRequest::Unminimize(h));
                        }
                    }
                    self.showing_desktop = false;
                } else {
                    let to_minimize: Vec<_> = self
                        .toplevels
                        .iter()
                        .filter(|(_, info)| {
                            !info
                                .state
                                .contains(&zcosmic_toplevel_handle_v1::State::Minimized)
                        })
                        .map(|(h, _)| h.clone())
                        .collect();

                    if !to_minimize.is_empty() {
                        if let Some(tx) = &self.tx {
                            for h in &to_minimize {
                                let _ = tx.send(WaylandRequest::Minimize(h.clone()));
                                self.hidden_by_us.insert(h.clone());
                            }
                        }
                        self.showing_desktop = true;
                    }
                }
            }
            Message::Wayland(update) => match update {
                WaylandUpdate::Init(tx) => {
                    self.tx = Some(tx);
                }
                WaylandUpdate::Finished => {
                    tracing::error!("Wayland handler exited");
                }
                WaylandUpdate::Toplevel(t) => match t {
                    ToplevelUpdate::Add(handle, info) => {
                        if !self.toplevels.iter().any(|(h, _)| *h == handle) {
                            self.toplevels.push((handle, info));
                        }
                    }
                    ToplevelUpdate::Update(handle, info) => {
                        if let Some((_, existing)) =
                            self.toplevels.iter_mut().find(|(h, _)| *h == handle)
                        {
                            *existing = info;
                        }
                    }
                    ToplevelUpdate::Remove(handle) => {
                        self.hidden_by_us.remove(&handle);
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
