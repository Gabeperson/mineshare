use super::consts::*;
use super::types::*;
use crate::PROTOCOL_VERSION;

use flume::{RecvTimeoutError, Selector};
use jiff::Timestamp;
use rand::Rng as _;
use ratatui::{
    crossterm::event::{Event as CrosstermEvent, KeyCode, KeyModifiers, MouseEventKind},
    layout::Position,
};
use std::time::Duration;
use std::{
    collections::{HashSet, VecDeque},
    net::{Ipv4Addr, SocketAddr},
    time::Instant,
};
use tokio::{net::UdpSocket, runtime::Handle};

#[must_use]
pub fn spawn_server_list_checker(handle: &Handle) -> (flume::Receiver<ServerInfo>, AbortOnDrop) {
    let (send, recv) = flume::bounded(10);
    let handle = handle.spawn(async move {
        let Ok(udp) = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 4445))).await else {
            return;
        };
        if let Err(_e) = udp.join_multicast_v4(Ipv4Addr::new(224, 0, 2, 60), Ipv4Addr::UNSPECIFIED)
        {
            return;
        }
        let mut buf = [0u8; 1024];
        let mut set = HashSet::new();
        loop {
            const MOTD_START: &str = "[MOTD]";
            const MOTD_END: &str = "[/MOTD]";
            const ADDR_START: &str = "[AD]";
            const ADDR_END: &str = "[/AD]";
            let Ok((len, from)) = udp.recv_from(&mut buf).await else {
                break;
            };
            let Ok(s) = str::from_utf8(&buf[..len]) else {
                continue;
            };
            let Some(motd_start) = s.find(MOTD_START) else {
                break;
            };
            let Some(motd_end) = s.find(MOTD_END) else {
                break;
            };
            let Some(addr_start) = s.find(ADDR_START) else {
                break;
            };
            let Some(addr_end) = s.find(ADDR_END) else {
                break;
            };
            let motd = &s[motd_start + MOTD_START.len()..motd_end];
            let port = &s[addr_start + ADDR_START.len()..addr_end];
            let port = port.parse::<u16>().unwrap_or(25565);
            let server_info = ServerInfo {
                motd: motd.to_owned(),
                ip: format!("{}:{port}", from.ip()),
            };
            if !set.contains(&server_info) {
                if let Err(_e) = send.send_async(server_info.clone()).await {
                    break;
                }
                set.insert(server_info);
            }
        }
    });
    (recv, handle.abort_on_drop())
}

impl TuiApp {
    // bool is whether we should continue
    pub fn handle_events(&mut self, render_result: RenderResult, frame_start: Instant) -> bool {
        let mut frame_end_time = frame_start + TICK_LEN;
        match render_result {
            RenderResult::MainMenu { bounding_boxes } => {
                let TuiAppState::MainMenu(state) = &mut self.app_state else {
                    unreachable!();
                };
                let mut redraw = false;
                loop {
                    let event = match self.recv_terminal_ev.recv_deadline(frame_end_time) {
                        Ok(e) => e,
                        Err(RecvTimeoutError::Timeout) => {
                            if redraw {
                                break;
                            }
                            frame_end_time = Instant::now() + TICK_LEN;
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => {
                            // If terminal polling failed, something has gone terribly wrong
                            return false;
                        }
                    };
                    let res = Self::handle_menu_event(
                        event,
                        &mut self.permanent_state,
                        &mut self.mouse_pos,
                        state,
                        &bounding_boxes,
                    );
                    match res {
                        MainMenuEventResult::Exit => return false,
                        MainMenuEventResult::Start => {
                            let Ok(connect_options) =
                                Self::handle_start(&self.permanent_state, &mut state.errors)
                            else {
                                return true;
                            };
                            self.send_ui_ev
                                .send(UiEvent::Start(connect_options))
                                .expect("Server thread should always be alive");
                            self.app_state = TuiAppState::ServerLoading(LoadingState {
                                cancel_selected: false,
                                retry: RETRY_START,
                            });
                            return true;
                        }
                        MainMenuEventResult::Redraw => {
                            redraw = true;
                        }
                        MainMenuEventResult::Ok => (),
                    }
                }
                while let Ok(server_info) = state.servers_recv.0.try_recv() {
                    state.servers.push(server_info);
                }
            }
            RenderResult::Loading {
                cancel_bounding_box,
            } => {
                enum RecvStatus {
                    ServerEvent(ServerEvent),
                    Crossterm(CrosstermEvent),
                }
                let TuiAppState::ServerLoading(state) = &mut self.app_state else {
                    unreachable!();
                };
                let mut redraw = false;
                loop {
                    let selector = Selector::new()
                        .recv(&self.recv_server_ev, |r| {
                            r.map_err(|_| ()).map(RecvStatus::ServerEvent)
                        })
                        .recv(&self.recv_terminal_ev, |r| {
                            r.map_err(|_| ()).map(RecvStatus::Crossterm)
                        });
                    let event = match selector.wait_deadline(frame_end_time) {
                        Ok(e) => e,
                        Err(_timeout) => {
                            if redraw {
                                break;
                            }
                            frame_end_time = Instant::now() + TICK_LEN;
                            continue;
                        }
                    };
                    let Ok(event) = event else {
                        // If one of the terminal ev or server ev channels is disconnected
                        // it means something's gone terribly wrong
                        return false;
                    };
                    match event {
                        RecvStatus::ServerEvent(server_event) => {
                            match server_event {
                                ServerEvent::Url(url) => {
                                    self.app_state = TuiAppState::Running(RunningState {
                                        ip: url,
                                        players: VecDeque::new(),
                                        logs: VecDeque::new(),
                                        scroll_players: 0,
                                        scroll_logs: 0,
                                        copied: false,
                                    });
                                    return true;
                                }
                                ServerEvent::ServerConnectionFailed(msg) => {
                                    self.app_state = TuiAppState::Failed(FailedState {
                                        permanent: false,
                                        msg,
                                        retry: state.retry,
                                        retry_at: Instant::now()
                                            + Duration::from_secs_f32(state.retry),
                                        selected: FailedSelected::None,
                                    });
                                    return true;
                                }
                                ServerEvent::ServerConnectionFailedTimeout => {
                                    self.app_state = TuiAppState::Failed(FailedState {
                                        permanent: false,
                                        msg: String::from("Timed out when connecting to proxy"),
                                        retry: state.retry,
                                        retry_at: Instant::now()
                                            + Duration::from_secs_f32(state.retry),
                                        selected: FailedSelected::None,
                                    });
                                    return true;
                                }
                                ServerEvent::InvalidProtocolVersion(protocol_version) => {
                                    let msg = if protocol_version > PROTOCOL_VERSION {
                                        format!(
                                            "Protocol version too low! You are on {PROTOCOL_VERSION} but proxy is on {protocol_version}"
                                        )
                                    } else {
                                        format!(
                                            "Protocol version too high! You are on {PROTOCOL_VERSION} but proxy is on {protocol_version}"
                                        )
                                    };
                                    self.app_state = TuiAppState::Failed(FailedState {
                                        permanent: true,
                                        msg,
                                        retry: 0.0,
                                        retry_at: Instant::now(),
                                        selected: FailedSelected::None,
                                    });
                                    return true;
                                }
                                ServerEvent::DidntGetRequestedUrl(url) => {
                                    self.app_state = TuiAppState::Failed(FailedState {
                                        permanent: true,
                                        msg: format!(
                                            "Did not get assigned requested subdomain. Got assigned `{url}`"
                                        ),
                                        retry: 0.0,
                                        retry_at: Instant::now(),
                                        selected: FailedSelected::None,
                                    });
                                    return true;
                                }
                                ServerEvent::ServerNonCatastrophicError(_)
                                | ServerEvent::MCServerConnectionFailed(_)
                                | ServerEvent::PlayerConnected(_)
                                | ServerEvent::PlayerDisconnected(_)
                                | ServerEvent::Stopped => {
                                    // These events shouldn't get send while connecting, but we might have built up events from
                                    // the previous connection, so we don't do a unreachable!() so we don't crash in that case
                                }
                            }
                        }
                        RecvStatus::Crossterm(event) => match event {
                            CrosstermEvent::Key(event) => {
                                if event.is_release() {
                                    continue;
                                }
                                match event.code {
                                    KeyCode::Tab | KeyCode::BackTab => {
                                        state.cancel_selected = true;
                                        redraw = true;
                                    }
                                    KeyCode::Esc => {
                                        state.cancel_selected = false;
                                        redraw = true;
                                    }
                                    KeyCode::Enter => {
                                        if state.cancel_selected {
                                            _ = self.send_ui_ev.send(UiEvent::Stop);
                                            let app_state =
                                                MainMenuTemporaryState::new(&self.tokio_handle);
                                            self.app_state = TuiAppState::MainMenu(app_state);
                                            return true;
                                        }
                                    }
                                    KeyCode::Char(c) => {
                                        if c == 'q'
                                            || (c == 'c'
                                                && event.modifiers.contains(KeyModifiers::CONTROL))
                                        {
                                            return false;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            CrosstermEvent::Mouse(event) => match event.kind {
                                MouseEventKind::Moved => {
                                    self.mouse_pos = (event.column, event.row);
                                    redraw = true;
                                }
                                MouseEventKind::Up(_button) => {
                                    self.mouse_pos = (u16::MAX, u16::MAX);
                                    if cancel_bounding_box
                                        .contains(Position::new(event.column, event.row))
                                    {
                                        _ = self.send_ui_ev.send(UiEvent::Stop);
                                        let app_state =
                                            MainMenuTemporaryState::new(&self.tokio_handle);
                                        self.app_state = TuiAppState::MainMenu(app_state);
                                        return true;
                                    }
                                    redraw = true;
                                }
                                _ => {}
                            },
                            _ => {}
                        },
                    }
                }
            }
            RenderResult::None => unreachable!(),
            RenderResult::FailScreen {
                main_menu,
                retry_now,
            } => {
                let TuiAppState::Failed(state) = &mut self.app_state else {
                    unreachable!();
                };
                loop {
                    let event = match self.recv_terminal_ev.recv_deadline(frame_end_time) {
                        Ok(e) => e,
                        Err(_timeout) => {
                            if Instant::now() >= state.retry_at && !state.permanent {
                                let connect_options =
                                    Self::handle_start(&self.permanent_state, &mut Vec::new())
                                        .expect(
                                            "We shouldn't be reconnecting if we errored at 'Start'",
                                        );
                                self.send_ui_ev
                                    .send(UiEvent::Start(connect_options))
                                    .expect("Server thread should always be alive");
                                self.app_state = TuiAppState::ServerLoading(LoadingState {
                                    cancel_selected: false,
                                    retry: (state.retry * RETRY_MULT).min(RETRY_MAX)
                                        * rand::rng().random_range(0.8..1.2),
                                });
                                return true;
                            }
                            break;
                        }
                    };
                    match event {
                        CrosstermEvent::Key(event) => {
                            if event.is_release() {
                                continue;
                            }
                            match event.code {
                                KeyCode::Tab => {
                                    state.selected = match state.selected {
                                        FailedSelected::None | FailedSelected::Retry => {
                                            FailedSelected::Cancel
                                        }
                                        FailedSelected::Cancel => {
                                            if state.permanent {
                                                FailedSelected::Cancel
                                            } else {
                                                FailedSelected::Retry
                                            }
                                        }
                                    };
                                }
                                KeyCode::BackTab => {
                                    state.selected = match state.selected {
                                        FailedSelected::None => FailedSelected::Retry,
                                        FailedSelected::Cancel => {
                                            if state.permanent {
                                                FailedSelected::Cancel
                                            } else {
                                                FailedSelected::Retry
                                            }
                                        }
                                        FailedSelected::Retry => FailedSelected::Cancel,
                                    };
                                }
                                KeyCode::Esc => {
                                    state.selected = FailedSelected::None;
                                }
                                KeyCode::Enter => match state.selected {
                                    FailedSelected::None => {}
                                    FailedSelected::Cancel => {
                                        let app_state =
                                            MainMenuTemporaryState::new(&self.tokio_handle);
                                        self.app_state = TuiAppState::MainMenu(app_state);
                                        return true;
                                    }
                                    FailedSelected::Retry => {
                                        let connect_options = Self::handle_start(
                                            &self.permanent_state,
                                            &mut Vec::new(),
                                        )
                                        .expect(
                                            "We shouldn't be reconnecting if we errored at 'Start'",
                                        );
                                        self.send_ui_ev
                                            .send(UiEvent::Start(connect_options))
                                            .expect("Server thread should always be alive");
                                        self.app_state = TuiAppState::ServerLoading(LoadingState {
                                            cancel_selected: false,
                                            retry: (state.retry * RETRY_MULT).min(RETRY_MAX)
                                                * rand::rng().random_range(0.8..1.2),
                                        });
                                        return true;
                                    }
                                },
                                KeyCode::Char(c) => {
                                    if c == 'q'
                                        || (c == 'c'
                                            && event.modifiers.contains(KeyModifiers::CONTROL))
                                    {
                                        return false;
                                    }
                                }
                                _ => {}
                            }
                        }
                        CrosstermEvent::Mouse(event) => match event.kind {
                            MouseEventKind::Moved => {
                                self.mouse_pos = (event.column, event.row);
                            }
                            MouseEventKind::Up(_button) => {
                                self.mouse_pos = (u16::MAX, u16::MAX);
                                let mouse_pos = Position::new(event.column, event.row);
                                if main_menu.contains(mouse_pos) {
                                    let app_state = MainMenuTemporaryState::new(&self.tokio_handle);
                                    self.app_state = TuiAppState::MainMenu(app_state);
                                    return true;
                                }
                                if let Some(retry) = retry_now
                                    && retry.contains(mouse_pos)
                                {
                                    {
                                        let connect_options = Self::handle_start(
                                            &self.permanent_state,
                                            &mut Vec::new(),
                                        )
                                        .expect(
                                            "We shouldn't be reconnecting if we errored at 'Start'",
                                        );
                                        self.send_ui_ev
                                            .send(UiEvent::Start(connect_options))
                                            .expect("Server thread should always be alive");
                                        self.app_state = TuiAppState::ServerLoading(LoadingState {
                                            cancel_selected: false,
                                            retry: (state.retry * RETRY_MULT).min(RETRY_MAX)
                                                * rand::rng().random_range(0.8..1.2),
                                        });
                                        return true;
                                    }
                                }
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                }
            }
            RenderResult::RunningScreen { bounding_boxes } => {
                enum RecvStatus {
                    ServerEvent(ServerEvent),
                    Crossterm(CrosstermEvent),
                }
                let TuiAppState::Running(state) = &mut self.app_state else {
                    unreachable!();
                };
                let mut redraw = false;
                'outer: loop {
                    let selector = Selector::new()
                        .recv(&self.recv_server_ev, |r| {
                            r.map_err(|_| ()).map(RecvStatus::ServerEvent)
                        })
                        .recv(&self.recv_terminal_ev, |r| {
                            r.map_err(|_| ()).map(RecvStatus::Crossterm)
                        });
                    let event = match selector.wait_deadline(frame_end_time) {
                        Ok(e) => e,
                        Err(_timeout) => {
                            if redraw {
                                break;
                            }
                            frame_end_time = Instant::now() + TICK_LEN;
                            continue;
                        }
                    };
                    let Ok(event) = event else {
                        // If one of the terminal ev or server ev channels is disconnected
                        // it means something's gone terribly wrong
                        return false;
                    };
                    match event {
                        RecvStatus::ServerEvent(server_event) => {
                            match server_event {
                                ServerEvent::ServerConnectionFailed(msg) => {
                                    self.app_state = TuiAppState::Failed(FailedState {
                                        permanent: false,
                                        msg,
                                        retry: RETRY_START,
                                        retry_at: Instant::now()
                                            + Duration::from_secs_f32(RETRY_START),
                                        selected: FailedSelected::None,
                                    });
                                    return true;
                                }
                                ServerEvent::MCServerConnectionFailed(msg) => {
                                    state.logs.push_front((Timestamp::now(), format!("Failed to connect to Minecraft server: {msg}. Is the MC server up?")));
                                    if state.logs.len() > 100 {
                                        state.logs.pop_back();
                                    }
                                    redraw = true;
                                }
                                ServerEvent::ServerNonCatastrophicError(msg) => {
                                    state
                                        .logs
                                        .push_front((Timestamp::now(), format!("Error: {msg}")));
                                    if state.logs.len() > 100 {
                                        state.logs.pop_back();
                                    }
                                    redraw = true;
                                }
                                ServerEvent::PlayerConnected(addr) => {
                                    state.logs.push_front((
                                        Timestamp::now(),
                                        format!("{addr} connected"),
                                    ));
                                    if state.logs.len() > 100 {
                                        state.logs.pop_back();
                                    }
                                    state.players.push_back(PlayerInfo { addr });
                                    redraw = true;
                                }
                                ServerEvent::PlayerDisconnected(addr) => {
                                    state.logs.push_front((
                                        Timestamp::now(),
                                        format!("{addr} disconnected"),
                                    ));
                                    if state.logs.len() > 100 {
                                        state.logs.pop_back();
                                    }
                                    for (i, player) in state.players.iter().enumerate() {
                                        if player.addr == addr {
                                            state.players.remove(i);
                                            if state.scroll_players > i {
                                                state.scroll_players -= 1;
                                            }
                                            redraw = true;
                                            continue 'outer;
                                        }
                                    }
                                }
                                ServerEvent::Stopped => {
                                    let app_state = MainMenuTemporaryState::new(&self.tokio_handle);
                                    self.app_state = TuiAppState::MainMenu(app_state);
                                    return true;
                                }
                                _ => {
                                    // These events shouldn't get send while connecting, but we might have built up events from
                                    // the previous connection, so we don't do a unreachable!() so we don't crash in that case
                                }
                            }
                        }
                        RecvStatus::Crossterm(event) => match event {
                            CrosstermEvent::Key(event) => {
                                if event.is_release() {
                                    continue;
                                }
                                if let KeyCode::Char(c) = event.code {
                                    if c == 'q' {
                                        _ = self.send_ui_ev.send(UiEvent::Stop);
                                        let app_state =
                                            MainMenuTemporaryState::new(&self.tokio_handle);
                                        self.app_state = TuiAppState::MainMenu(app_state);
                                        return true;
                                    } else if c == 'c'
                                        && event.modifiers.contains(KeyModifiers::CONTROL)
                                    {
                                        return false;
                                    }
                                }
                            }
                            CrosstermEvent::Mouse(event) => match event.kind {
                                MouseEventKind::Moved => {
                                    self.mouse_pos = (event.column, event.row);
                                    redraw = true;
                                }
                                MouseEventKind::Up(_button) => {
                                    self.mouse_pos = (u16::MAX, u16::MAX);
                                    let pos = Position::new(event.column, event.row);
                                    state.copied = false;
                                    if bounding_boxes.ip_box.contains(pos) {
                                        let Ok(mut clipboard) = arboard::Clipboard::new() else {
                                            continue;
                                        };
                                        if let Err(_e) = clipboard.set_text(&state.ip) {
                                            continue;
                                        }
                                        state.copied = true;
                                        return true;
                                    }
                                    for (addr, rect) in &bounding_boxes.disconnects {
                                        if rect.contains(pos) {
                                            _ = self.send_ui_ev.send(UiEvent::Disconnect(*addr));
                                            return true;
                                        }
                                    }
                                    redraw = true;
                                }
                                MouseEventKind::ScrollUp => {
                                    let pos = Position::new(event.column, event.row);
                                    if bounding_boxes.players_box.contains(pos) {
                                        let scroll = if state.players.len()
                                            <= (bounding_boxes.players_box.height) as usize
                                        {
                                            0
                                        } else {
                                            state.scroll_players.saturating_sub(1)
                                        };
                                        state.scroll_players = scroll;
                                        redraw = true;
                                    } else if bounding_boxes.logs_box.contains(pos) {
                                        let scroll = if state.logs.len()
                                            <= (bounding_boxes.logs_box.height) as usize
                                        {
                                            0
                                        } else {
                                            (state.scroll_logs.saturating_add(1)).min(
                                                state.logs.len().saturating_sub(
                                                    bounding_boxes.logs_box.height as usize,
                                                ),
                                            )
                                        };
                                        state.scroll_logs = scroll;
                                        redraw = true;
                                    }
                                }
                                MouseEventKind::ScrollDown => {
                                    let pos = Position::new(event.column, event.row);
                                    if bounding_boxes.players_box.contains(pos) {
                                        let scroll = if state.players.len()
                                            <= (bounding_boxes.players_box.height) as usize
                                        {
                                            0
                                        } else {
                                            (state.scroll_players.saturating_add(1)).min(
                                                state.players.len().saturating_sub(
                                                    bounding_boxes.players_box.height as usize,
                                                ),
                                            )
                                        };
                                        state.scroll_players = scroll;
                                        redraw = true;
                                    } else if bounding_boxes.logs_box.contains(pos) {
                                        let scroll = if state.logs.len()
                                            <= (bounding_boxes.logs_box.height) as usize
                                        {
                                            0
                                        } else {
                                            state.scroll_logs.saturating_sub(1)
                                        };
                                        state.scroll_logs = scroll;
                                        redraw = true;
                                    }
                                }

                                _ => {}
                            },
                            _ => {}
                        },
                    }
                }
            }
        }
        true
    }

    fn handle_start(
        state: &MainMenuPermanentState,
        errors: &mut Vec<String>,
    ) -> Result<ConnectOptions, ()> {
        let MainMenuPermanentState {
            proxy_server,
            proxy_server_play_port,
            proxy_server_init_port,
            server_ip,
            advanced: _,
            request_domain,
        } = &state;
        errors.clear();
        if server_ip.is_empty() {
            errors.push(String::from("Server IP should not be empty!"));
        }
        let proxy_server_play_port = if let Ok(p) = proxy_server_play_port.parse::<u16>() {
            p
        } else {
            errors.push(format!(
                "Failed to parse proxy server play port: {proxy_server_play_port}"
            ));
            0
        };
        let proxy_server_init_port = if let Ok(p) = proxy_server_init_port.parse::<u16>() {
            p
        } else {
            errors.push(format!(
                "Failed to parse proxy server init port: {proxy_server_init_port}"
            ));
            0
        };
        if !errors.is_empty() {
            return Err(());
        }
        let request_domain = if request_domain.is_empty() {
            None
        } else {
            Some(request_domain.clone())
        };
        Ok(ConnectOptions {
            proxy_server: proxy_server.clone(),
            proxy_server_play_port,
            proxy_server_init_port,
            server_ip: server_ip.clone(),
            request_domain,
        })
    }
    #[allow(clippy::too_many_lines)]
    fn handle_menu_event(
        e: CrosstermEvent,
        perm_state: &mut MainMenuPermanentState,
        mouse_pos: &mut (u16, u16),
        temp_state: &mut MainMenuTemporaryState,
        bounding_boxes: &MainMenuBoundingBoxes,
    ) -> MainMenuEventResult {
        match e {
            CrosstermEvent::Key(e) => {
                let edit_string = match temp_state.selected {
                    SelectedBlock::ServerIp => Some(&mut perm_state.server_ip),
                    SelectedBlock::RequestedDomain => Some(&mut perm_state.request_domain),
                    SelectedBlock::ProxyIp => Some(&mut perm_state.proxy_server),
                    SelectedBlock::PlayPort => Some(&mut perm_state.proxy_server_play_port),
                    SelectedBlock::InitPort => Some(&mut perm_state.proxy_server_init_port),
                    _ => None,
                };
                if e.is_release() {
                    return MainMenuEventResult::Ok;
                }
                match e.code {
                    KeyCode::Backspace => {
                        if let Some(s) = edit_string {
                            match temp_state.cursor {
                                Cursor::Position(cursor) => {
                                    if cursor > 0 && !s.is_empty() {
                                        s.remove(cursor - 1);
                                        temp_state.cursor -= 1;
                                        return MainMenuEventResult::Redraw;
                                    }
                                }
                                Cursor::All => {
                                    s.clear();
                                    temp_state.cursor = Cursor::Position(0);
                                    return MainMenuEventResult::Redraw;
                                }
                            }
                        }
                    }
                    KeyCode::Down | KeyCode::Tab => {
                        let next_select = match temp_state.selected {
                            SelectedBlock::ServerIp => SelectedBlock::RequestedDomain,
                            SelectedBlock::RequestedDomain => SelectedBlock::AdvancedButton,
                            SelectedBlock::ProxyIp => SelectedBlock::PlayPort,
                            SelectedBlock::PlayPort => SelectedBlock::InitPort,
                            SelectedBlock::InitPort => SelectedBlock::StartButton,
                            SelectedBlock::AdvancedButton => {
                                if perm_state.advanced {
                                    SelectedBlock::ProxyIp
                                } else {
                                    SelectedBlock::StartButton
                                }
                            }
                            SelectedBlock::None | SelectedBlock::StartButton => {
                                SelectedBlock::ServerIp
                            }
                        };
                        *mouse_pos = (u16::MAX, u16::MAX);
                        temp_state.selected = next_select;
                        temp_state.set_cursor(perm_state);
                        return MainMenuEventResult::Redraw;
                    }
                    KeyCode::Left => {
                        if let Some(_s) = edit_string {
                            match temp_state.cursor {
                                Cursor::Position(cursor) => {
                                    if cursor > 0 {
                                        temp_state.cursor -= 1;
                                        return MainMenuEventResult::Redraw;
                                    }
                                }
                                Cursor::All => {
                                    temp_state.cursor = Cursor::Position(0);
                                    return MainMenuEventResult::Redraw;
                                }
                            }
                        }
                    }
                    KeyCode::Right => {
                        if let Some(s) = edit_string {
                            match temp_state.cursor {
                                Cursor::Position(cursor) => {
                                    if cursor < s.len() {
                                        temp_state.cursor += 1;
                                        return MainMenuEventResult::Redraw;
                                    }
                                }
                                Cursor::All => {
                                    temp_state.set_cursor(perm_state);
                                    return MainMenuEventResult::Redraw;
                                }
                            }
                        }
                    }
                    KeyCode::Up | KeyCode::BackTab => {
                        let next_select = match temp_state.selected {
                            SelectedBlock::None | SelectedBlock::ServerIp => {
                                SelectedBlock::StartButton
                            }
                            SelectedBlock::RequestedDomain => SelectedBlock::ServerIp,
                            SelectedBlock::ProxyIp => SelectedBlock::AdvancedButton,
                            SelectedBlock::PlayPort => SelectedBlock::ProxyIp,
                            SelectedBlock::InitPort => SelectedBlock::PlayPort,
                            SelectedBlock::AdvancedButton => SelectedBlock::RequestedDomain,
                            SelectedBlock::StartButton => {
                                if perm_state.advanced {
                                    SelectedBlock::InitPort
                                } else {
                                    SelectedBlock::AdvancedButton
                                }
                            }
                        };
                        *mouse_pos = (u16::MAX, u16::MAX);
                        temp_state.selected = next_select;
                        temp_state.set_cursor(perm_state);
                        return MainMenuEventResult::Redraw;
                    }
                    KeyCode::Home => {
                        if let Some(_s) = edit_string {
                            temp_state.cursor = Cursor::Position(0);
                            return MainMenuEventResult::Redraw;
                        }
                    }
                    KeyCode::End => {
                        if let Some(_s) = edit_string {
                            temp_state.set_cursor(perm_state);
                            return MainMenuEventResult::Redraw;
                        }
                    }
                    KeyCode::Delete => {
                        if let Some(s) = edit_string {
                            match temp_state.cursor {
                                Cursor::Position(cursor) => {
                                    if cursor < s.len() {
                                        s.remove(cursor);
                                        return MainMenuEventResult::Redraw;
                                    }
                                }
                                Cursor::All => {
                                    s.clear();
                                    temp_state.cursor = Cursor::Position(0);
                                    return MainMenuEventResult::Redraw;
                                }
                            }
                        }
                    }
                    KeyCode::Char(c)
                        if c.is_ascii_alphanumeric() || c == '-' || c == ':' || c == '.' =>
                    {
                        if c == 'c' && e.modifiers.contains(KeyModifiers::CONTROL) {
                            return MainMenuEventResult::Exit;
                        } else if c == 'a' && e.modifiers.contains(KeyModifiers::CONTROL) {
                            temp_state.cursor = Cursor::All;
                            return MainMenuEventResult::Redraw;
                        }
                        if let Some(s) = edit_string {
                            match temp_state.cursor {
                                Cursor::Position(cursor) => {
                                    s.insert(cursor, c);
                                    temp_state.cursor += 1;
                                }
                                Cursor::All => {
                                    s.clear();
                                    s.push(c);
                                    temp_state.set_cursor(perm_state);
                                }
                            }
                            return MainMenuEventResult::Redraw;
                        }
                        if c == 'q' {
                            return MainMenuEventResult::Exit;
                        }
                    }
                    KeyCode::Esc => {
                        temp_state.selected = SelectedBlock::None;
                        temp_state.cursor = Cursor::Position(0);
                        return MainMenuEventResult::Redraw;
                    }
                    KeyCode::Enter => match temp_state.selected {
                        SelectedBlock::AdvancedButton => {
                            perm_state.advanced = !perm_state.advanced;
                            return MainMenuEventResult::Redraw;
                        }
                        SelectedBlock::StartButton => return MainMenuEventResult::Start,
                        _ => {}
                    },
                    _ => (),
                }
                MainMenuEventResult::Ok
            }
            CrosstermEvent::Mouse(e) => match e.kind {
                MouseEventKind::Up(_mouse_button) => {
                    *mouse_pos = (u16::MAX, u16::MAX);
                    temp_state.last_selected_server = None;
                    let pos = Position::new(e.column, e.row);
                    let MainMenuBoundingBoxes {
                        ip_rect: ip,
                        domain_rect: domain,
                        advanced_rect: advanced,
                        advanced_rects: advanced_textboxes,
                        start_rect: start,
                        server_rects: servers,
                    } = &bounding_boxes;
                    if ip.contains(pos) {
                        temp_state.selected = SelectedBlock::ServerIp;
                        temp_state.set_cursor(perm_state);
                        return MainMenuEventResult::Redraw;
                    }
                    if domain.contains(pos) {
                        temp_state.selected = SelectedBlock::RequestedDomain;
                        temp_state.set_cursor(perm_state);
                        return MainMenuEventResult::Redraw;
                    }
                    if advanced.contains(pos) {
                        perm_state.advanced = !perm_state.advanced;
                        temp_state.selected = SelectedBlock::None;
                        return MainMenuEventResult::Redraw;
                    }
                    if start.contains(pos) {
                        return MainMenuEventResult::Start;
                    }
                    if let Some(AdvancedBoxes {
                        proxy_ip,
                        play_port,
                        init_port,
                    }) = advanced_textboxes
                    {
                        if proxy_ip.contains(pos) {
                            temp_state.selected = SelectedBlock::ProxyIp;
                            temp_state.set_cursor(perm_state);
                            return MainMenuEventResult::Redraw;
                        }
                        if play_port.contains(pos) {
                            temp_state.selected = SelectedBlock::PlayPort;
                            temp_state.set_cursor(perm_state);
                            return MainMenuEventResult::Redraw;
                        }
                        if init_port.contains(pos) {
                            temp_state.selected = SelectedBlock::InitPort;
                            temp_state.set_cursor(perm_state);
                            return MainMenuEventResult::Redraw;
                        }
                    }
                    let redraw = temp_state.selected != SelectedBlock::None;
                    temp_state.selected = SelectedBlock::None;
                    for (i, server) in servers.iter().enumerate() {
                        if server.contains(pos) {
                            perm_state.server_ip.clone_from(&temp_state.servers[i].ip);
                            temp_state.last_selected_server = Some(i);
                            temp_state.selected = SelectedBlock::ServerIp;
                            temp_state.set_cursor(perm_state);
                            return MainMenuEventResult::Redraw;
                        }
                    }
                    if redraw {
                        MainMenuEventResult::Redraw
                    } else {
                        MainMenuEventResult::Ok
                    }
                }
                MouseEventKind::Moved => {
                    *mouse_pos = (e.column, e.row);
                    MainMenuEventResult::Redraw
                }
                _ => MainMenuEventResult::Ok,
            },
            CrosstermEvent::Paste(s) => {
                let edit_string = match temp_state.selected {
                    SelectedBlock::None => None,
                    SelectedBlock::ServerIp => Some(&mut perm_state.server_ip),
                    SelectedBlock::RequestedDomain => Some(&mut perm_state.request_domain),
                    SelectedBlock::ProxyIp => Some(&mut perm_state.proxy_server),
                    SelectedBlock::PlayPort => Some(&mut perm_state.proxy_server_play_port),
                    SelectedBlock::InitPort => Some(&mut perm_state.proxy_server_init_port),
                    _ => return MainMenuEventResult::Ok,
                };
                let Some(edit_string) = edit_string else {
                    return MainMenuEventResult::Ok;
                };
                let allowed_only = s
                    .chars()
                    .filter(|&c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == ':')
                    .collect::<String>();
                match temp_state.cursor {
                    Cursor::Position(cursor) => {
                        edit_string.insert_str(cursor, &allowed_only);
                    }
                    Cursor::All => {
                        *edit_string = allowed_only;
                    }
                }

                MainMenuEventResult::Ok
            }
            _ => MainMenuEventResult::Ok,
        }
    }
}
