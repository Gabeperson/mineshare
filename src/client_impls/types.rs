use super::tui_events::spawn_server_list_checker;
use ratatui::{
    DefaultTerminal,
    crossterm::event::Event as CrosstermEvent,
    layout::{Position, Rect},
};
use std::{
    collections::VecDeque,
    net::SocketAddr,
    ops::{AddAssign, SubAssign},
    time::Instant,
};
use tokio::{runtime::Handle, task::AbortHandle};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub struct AbortOnDrop(AbortHandle);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
pub trait AbortOnDropExt {
    fn abort_on_drop(&self) -> AbortOnDrop;
}
impl<T> AbortOnDropExt for tokio::task::JoinHandle<T> {
    fn abort_on_drop(&self) -> AbortOnDrop {
        AbortOnDrop(self.abort_handle())
    }
}

#[derive(Debug, Clone)]
pub enum ServerEvent {
    ServerConnectionFailed(String),
    ServerNonCatastrophicError(String),
    ServerConnectionFailedTimeout,
    MCServerConnectionFailed(String),
    InvalidProtocolVersion(u64),
    Stopped,
    Url(String),
    DidntGetRequestedUrl(String),
    PlayerConnected(SocketAddr, String),
    PlayerDisconnected(SocketAddr, String),
    Pinged(SocketAddr),
}
#[derive(Debug)]
pub enum UiEvent {
    Stop,
    Start(ConnectOptions),
    Disconnect(SocketAddr),
}

#[derive(Debug)]
pub struct TuiApp {
    pub send_ui_ev: flume::Sender<UiEvent>,
    pub recv_server_ev: flume::Receiver<ServerEvent>,
    pub app_state: TuiAppState,
    pub tokio_handle: Handle,
    pub permanent_state: MainMenuPermanentState,
    pub mouse_pos: (u16, u16),
    pub recv_terminal_ev: flume::Receiver<CrosstermEvent>,
}
impl TuiApp {
    pub fn run(mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        loop {
            let mut render_result = RenderResult::None;
            let start = Instant::now();
            terminal.draw(|frame| render_result = self.render(frame))?;
            if !self.handle_events(render_result, start) {
                break;
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct MainMenuPermanentState {
    pub proxy_server: String,
    pub proxy_server_play_port: String,
    pub proxy_server_init_port: String,
    pub server_ip: String,
    pub advanced: bool,
    pub request_domain: String,
}

#[derive(Debug, Hash, PartialEq, Eq, Ord, PartialOrd)]
pub struct PlayerInfo {
    pub addr: SocketAddr,
    pub username: String,
}

#[derive(Debug)]
pub struct RunningState {
    pub ip: String,
    pub players: VecDeque<PlayerInfo>,
    pub logs: VecDeque<(jiff::Zoned, String)>,
    pub scroll_players: usize,
    pub scroll_logs: usize,
    pub copied: bool,
}

#[derive(Debug)]
pub struct LoadingState {
    pub cancel_selected: bool,
    pub retry: f32,
}

#[derive(Debug)]
pub enum FailedSelected {
    None,
    Cancel,
    Retry,
}

#[derive(Debug)]
pub struct FailedState {
    pub permanent: bool,
    pub msg: String,
    pub retry: f32,
    pub retry_at: Instant,
    pub selected: FailedSelected,
}

#[derive(Debug)]
pub enum TuiAppState {
    MainMenu(MainMenuTemporaryState),
    ServerLoading(LoadingState),
    Running(RunningState),
    Failed(FailedState),
}

#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub struct ServerInfo {
    pub motd: String,
    pub ip: String,
}

#[derive(Debug)]
pub struct MainMenuTemporaryState {
    pub servers_recv: (flume::Receiver<ServerInfo>, AbortOnDrop),
    pub servers: Vec<ServerInfo>,
    pub last_selected_server: Option<usize>,
    pub selected: SelectedBlock,
    pub cursor: Cursor,
    pub errors: Vec<String>,
}

impl MainMenuTemporaryState {
    pub fn set_cursor(&mut self, perm_state: &MainMenuPermanentState) {
        self.cursor = Cursor::Position(match self.selected {
            SelectedBlock::None => 0,
            SelectedBlock::ServerIp => perm_state.server_ip.len(),
            SelectedBlock::RequestedDomain => perm_state.request_domain.len(),
            SelectedBlock::ProxyIp => perm_state.proxy_server.len(),
            SelectedBlock::PlayPort => perm_state.proxy_server_play_port.len(),
            SelectedBlock::InitPort => perm_state.proxy_server_init_port.len(),
            _ => return,
        });
    }
    #[must_use]
    pub fn new(handle: &Handle) -> MainMenuTemporaryState {
        let servers_recv = spawn_server_list_checker(handle);
        MainMenuTemporaryState {
            servers_recv,
            servers: Vec::new(),
            last_selected_server: None,
            selected: SelectedBlock::None,
            cursor: Cursor::Position(0),
            errors: Vec::new(),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SelectedBlock {
    None,
    ServerIp,
    RequestedDomain,
    AdvancedButton,
    StartButton,
    ProxyIp,
    PlayPort,
    InitPort,
}

#[derive(Debug)]
pub struct MainMenuBoundingBoxes {
    pub ip_rect: Rect,
    pub domain_rect: Rect,
    pub advanced_rect: Rect,
    pub advanced_rects: Option<AdvancedBoxes>,
    pub start_rect: Rect,
    pub server_rects: Vec<Rect>,
}

#[derive(Debug)]
pub struct AdvancedBoxes {
    pub proxy_ip: Rect,
    pub play_port: Rect,
    pub init_port: Rect,
}

#[derive(Debug)]
pub struct RunningBoundingBoxes {
    pub disconnects: Vec<(SocketAddr, Rect)>,
    pub ip_box: Rect,
    pub players_box: Rect,
    pub logs_box: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cursor {
    Position(usize),
    All,
}

impl SubAssign<usize> for Cursor {
    fn sub_assign(&mut self, rhs: usize) {
        if let Cursor::Position(pos) = self {
            *pos -= rhs;
        }
    }
}

impl AddAssign<usize> for Cursor {
    fn add_assign(&mut self, rhs: usize) {
        if let Cursor::Position(pos) = self {
            *pos += rhs;
        }
    }
}

pub struct EditableTextBox<'a> {
    pub title: &'a str,
    pub content: &'a str,
    pub selected: bool,
    pub cursor: Cursor,
    pub mouse_position: Position,
    pub rect: Rect,
    pub suffix: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainMenuEventResult {
    Ok,
    Redraw,
    Exit,
    Start,
}

pub enum RenderResult {
    MainMenu {
        bounding_boxes: MainMenuBoundingBoxes,
    },
    Loading {
        cancel_bounding_box: Rect,
    },
    FailScreen {
        main_menu: Rect,
        retry_now: Option<Rect>,
    },
    RunningScreen {
        bounding_boxes: RunningBoundingBoxes,
    },
    None,
}

#[derive(Debug)]
pub struct ConnectOptions {
    pub proxy_server: String,
    pub proxy_server_play_port: u16,
    pub proxy_server_init_port: u16,
    pub server_ip: String,
    pub request_domain: Option<String>,
}

#[derive(Debug)]
pub enum ClientStatusUpdate {
    Connected {
        addr: SocketAddr,
        token: CancellationToken,
    },
    Disconnected {
        addr: SocketAddr,
    },
}
