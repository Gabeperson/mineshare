#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::wildcard_imports)]
#![allow(clippy::too_many_lines)]

use mineshare::client_impls::{consts::DEFAULT_URL, *};
use types::*;

use ratatui::crossterm::{
    self, ExecutableCommand,
    event::{DisableMouseCapture, EnableMouseCapture},
};
use std::time::Duration;

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let handle = rt.handle();
    let (send, recv_server) = flume::bounded(10);
    let (send_server, recv) = flume::bounded(10);
    let (send_terminal_ev, recv_terminal_ev) = flume::bounded(10);
    let _terminal_poll_thread = std::thread::spawn(move || {
        loop {
            let Ok(ev_exists) = crossterm::event::poll(Duration::MAX) else {
                return;
            };
            if !ev_exists {
                continue;
            }
            let Ok(e) = crossterm::event::read() else {
                return;
            };
            if send_terminal_ev.send(e).is_err() {
                return;
            }
        }
    });
    rt.spawn(server::server_thread(recv_server, send_server));
    set_panic_hook();
    ratatui::run(|terminal| {
        std::io::stdout().execute(EnableMouseCapture).unwrap();
        let app_state = MainMenuTemporaryState::new(handle);
        TuiApp {
            recv_terminal_ev,
            send_ui_ev: send,
            recv_server_ev: recv,
            tokio_handle: handle.clone(),
            app_state: TuiAppState::MainMenu(app_state),
            permanent_state: MainMenuPermanentState {
                advanced: false,
                proxy_server: String::from(DEFAULT_URL),
                proxy_server_play_port: String::from("25564"),
                proxy_server_init_port: String::from("25563"),
                server_ip: String::new(),
                request_domain: String::new(),
            },
            mouse_pos: (0, 0),
        }
        .run(terminal)
        .unwrap();
        std::io::stdout().execute(DisableMouseCapture).unwrap();
    });
}

fn set_panic_hook() {
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |pinfo| {
        std::io::stdout().execute(EnableMouseCapture).unwrap();
        prev_hook(pinfo);
    }));
}
