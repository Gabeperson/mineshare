use std::time::Instant;

use super::consts::*;
use super::types::*;

use ratatui::layout::Flex;
use ratatui::layout::Spacing;
use ratatui::widgets::Wrap;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Position, Rect},
    style::Style,
    text::{Line, Span, Text},
    widgets::{Block, Paragraph},
};

impl EditableTextBox<'_> {
    pub fn render(self, frame: &mut Frame) {
        let inner_rect = self.rect.inner(Margin::new(1, 1));
        let block = Block::bordered()
            .title(self.title)
            .border_style(if self.selected {
                Style::new().light_green()
            } else if self.rect.contains(self.mouse_position) {
                Style::new().light_magenta()
            } else {
                Style::new()
            });
        frame.render_widget(&block, self.rect);
        let paragraph = Paragraph::new(self.content);
        frame.render_widget(&paragraph, inner_rect);
        if self.selected {
            // Using default terminal cursor here bugs out windows terminal so we have to
            // use a custom cursor :(
            match self.cursor {
                Cursor::Position(cursor) => {
                    let cursor_content = self.content.get(cursor..cursor + 1).unwrap_or(" ");
                    let paragraph =
                        Paragraph::new(cursor_content).style(Style::new().black().on_green());
                    let area = Rect::new(inner_rect.left() + cursor as u16, inner_rect.top(), 1, 1);
                    frame.render_widget(paragraph, area);
                }
                Cursor::All => {
                    let cursor_content = self.content.get(..).unwrap_or(" ");
                    let paragraph =
                        Paragraph::new(cursor_content).style(Style::new().black().on_green());
                    let area = Rect::new(
                        inner_rect.left(),
                        inner_rect.top(),
                        self.content.len() as u16,
                        1,
                    );
                    frame.render_widget(paragraph, area);
                }
            }
        }
    }
}

impl TuiApp {
    pub fn render(&mut self, frame: &mut Frame) -> RenderResult {
        match &mut self.app_state {
            TuiAppState::MainMenu(state) => RenderResult::MainMenu {
                bounding_boxes: state.render(&mut self.permanent_state, frame, self.mouse_pos),
            },
            TuiAppState::ServerLoading(state) => RenderResult::Loading {
                cancel_bounding_box: state.render(frame, self.mouse_pos),
            },
            TuiAppState::Failed(state) => {
                let (main_menu, retry_now) = state.render(frame, self.mouse_pos);
                RenderResult::FailScreen {
                    main_menu,
                    retry_now,
                }
            }
            TuiAppState::Running(state) => RenderResult::RunningScreen {
                bounding_boxes: state.render(frame, self.mouse_pos),
            },
        }
    }
}

impl MainMenuTemporaryState {
    #[allow(clippy::too_many_lines)]
    fn render(
        &mut self,
        perm_state: &mut MainMenuPermanentState,
        frame: &mut Frame,
        mouse_pos: (u16, u16),
    ) -> MainMenuBoundingBoxes {
        let pos = Position::new(mouse_pos.0, mouse_pos.1);
        let area = frame.area();
        let [area, help_area] = area
            .try_layout(&Layout::vertical([
                Constraint::Fill(1),
                Constraint::Length(1),
            ]))
            .unwrap();
        let para = Paragraph::new(
            "Next: Tab, Previous: Shift+Tab, Exit: q, Select: Enter; (Can use mouse too!)",
        );
        frame.render_widget(para, help_area);
        let [left, right] = area
            .try_layout(&Layout::horizontal([
                Constraint::Percentage(50),
                Constraint::Percentage(50),
            ]))
            .unwrap();
        let left_inner = left.inner(Margin::new(1, 1));
        frame.render_widget(Block::bordered(), left);
        frame.render_widget(Block::bordered(), right);
        let [
            server_ip_rect,
            request_domain_rect,
            advanced_rect,
            advanced_text_rect,
            error_rem,
            start_rect,
        ] = left_inner
            .try_layout(&Layout::vertical([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Length(9),
                Constraint::Fill(1),
                Constraint::Length(3),
            ]))
            .unwrap();
        EditableTextBox {
            title: " Minecraft Server IP ",
            content: &perm_state.server_ip,
            selected: self.selected == SelectedBlock::ServerIp,
            cursor: self.cursor,
            mouse_position: pos,
            rect: server_ip_rect,
        }
        .render(frame);
        EditableTextBox {
            title: " Requested Domain (Optional) (Ex: test.mineshare.dev)",
            content: &perm_state.request_domain,
            selected: self.selected == SelectedBlock::RequestedDomain,
            cursor: self.cursor,
            mouse_position: pos,
            rect: request_domain_rect,
        }
        .render(frame);

        let advanced_layout =
            Layout::horizontal([Constraint::Length(ADVANCED.len() as u16)]).flex(Flex::Start);
        let [advanced_rect] = advanced_rect.try_layout(&advanced_layout).unwrap();
        let advanced_style =
            if self.selected == SelectedBlock::AdvancedButton || advanced_rect.contains(pos) {
                Style::new().black().on_light_green()
            } else {
                Style::new().black().on_light_blue()
            };
        let advanced = Paragraph::new(ADVANCED).block(Block::new().style(advanced_style));
        frame.render_widget(&advanced, advanced_rect);

        let start_layout = Layout::vertical([Constraint::Length(1)]).flex(Flex::Center);
        let start_style = if self.selected == SelectedBlock::StartButton || start_rect.contains(pos)
        {
            Style::new().black().on_light_green()
        } else {
            Style::new().black().on_light_blue()
        };
        frame.render_widget(Block::new().style(start_style), start_rect);
        let [start_inner_rect] = start_rect.try_layout(&start_layout).unwrap();
        let start = Paragraph::new(START)
            .centered()
            .block(Block::new().style(Style::new().black()));
        frame.render_widget(&start, start_inner_rect);
        let listitems = self.servers.iter().enumerate().map(|(i, info)| {
            let mut text = Text::default();
            let mut line1 = Line::default();
            line1.push_span(Span::styled("Lan Server", Style::new().light_blue()));
            line1.push_span(Span::raw(" - "));
            line1.push_span(Span::styled(&*info.ip, Style::new().light_green()));
            if self.last_selected_server == Some(i) {
                line1.push_span(Span::styled(
                    " - Set as server IP!",
                    Style::new().light_magenta(),
                ));
            }
            text.push_line(line1);
            text.push_line(Line::styled(&*info.motd, Style::new().white()));
            text
        });
        let layout_it = self
            .servers
            .iter()
            .map(|_| Constraint::Length(4))
            .chain(std::iter::once(Constraint::Length(3)));
        let right_inner = right.inner(Margin::new(1, 1));
        let blocks = Layout::vertical(layout_it)
            .flex(Flex::Start)
            .split(right_inner);
        let mut iter = blocks.iter();
        let loading = iter.next_back().expect("We chain guaranteed iterators");
        let [loading_rect] = loading
            .try_layout(&Layout::vertical([Constraint::Length(1)]).flex(Flex::Center))
            .unwrap();
        frame.render_widget(
            Paragraph::new("Loading LAN servers...").centered(),
            loading_rect,
        );

        let server_rects = iter.copied().collect::<Vec<_>>();
        for (rect, item) in server_rects.iter().zip(listitems) {
            let style = if rect.contains(pos) {
                Style::new().light_magenta()
            } else {
                Style::new()
            };
            let p = Paragraph::new(item).block(Block::bordered().style(style));
            frame.render_widget(p, *rect);
        }
        let advanced_rects = if perm_state.advanced {
            let [proxy_ip, play_port, init_port] = advanced_text_rect
                .try_layout(&Layout::vertical([
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Length(3),
                ]))
                .unwrap();
            EditableTextBox {
                title: " Proxy server IP ",
                content: &perm_state.proxy_server,
                selected: self.selected == SelectedBlock::ProxyIp,
                cursor: self.cursor,
                mouse_position: pos,
                rect: proxy_ip,
            }
            .render(frame);
            EditableTextBox {
                title: " Proxy server PLAY port ",
                content: &perm_state.proxy_server_play_port,
                selected: self.selected == SelectedBlock::PlayPort,
                cursor: self.cursor,
                mouse_position: pos,
                rect: play_port,
            }
            .render(frame);
            EditableTextBox {
                title: " Proxy server INIT port ",
                content: &perm_state.proxy_server_init_port,
                selected: self.selected == SelectedBlock::InitPort,
                cursor: self.cursor,
                mouse_position: pos,
                rect: init_port,
            }
            .render(frame);
            Some(AdvancedBoxes {
                proxy_ip,
                play_port,
                init_port,
            })
        } else {
            None
        };
        let err_rect = if advanced_rects.is_none() {
            advanced_text_rect.union(error_rem)
        } else {
            error_rem
        };
        if !self.errors.is_empty() {
            let mut text = Text::default();
            for error in &self.errors {
                text.push_line(&**error);
            }
            let error_paragraph = Paragraph::new(text).style(Style::new().red());
            frame.render_widget(error_paragraph, err_rect);
        }

        MainMenuBoundingBoxes {
            ip_rect: server_ip_rect,
            domain_rect: request_domain_rect,
            advanced_rect,
            start_rect,
            server_rects,
            advanced_rects,
        }
    }
}

impl LoadingState {
    fn render(&self, frame: &mut Frame, mouse_pos: (u16, u16)) -> Rect {
        const CANCEL: &str = " Cancel ";
        let mouse_pos = Position::new(mouse_pos.0, mouse_pos.1);
        let area = frame.area();
        let [area, help_area] = area
            .try_layout(&Layout::vertical([
                Constraint::Fill(1),
                Constraint::Length(1),
            ]))
            .unwrap();
        let para = Paragraph::new(
            "Next: Tab, Previous: Shift+Tab, Exit: q, Select: Enter; (Can use mouse too!)",
        );
        frame.render_widget(para, help_area);
        let [mid] = area
            .try_layout(&Layout::horizontal([Constraint::Percentage(25)]).flex(Flex::Center))
            .unwrap();
        let [loading, cancel] = mid
            .try_layout(
                &Layout::vertical([Constraint::Length(3), Constraint::Length(1)])
                    .spacing(Spacing::Space(1))
                    .flex(Flex::Center),
            )
            .unwrap();
        let [cancel] = cancel
            .try_layout(
                &Layout::horizontal([Constraint::Length(CANCEL.len() as u16)]).flex(Flex::Center),
            )
            .unwrap();
        let loading_para = Paragraph::new(" Loading...")
            .centered()
            .block(Block::bordered());
        frame.render_widget(loading_para, loading);
        let cancel_style = if cancel.contains(mouse_pos) || self.cancel_selected {
            Style::new().on_magenta()
        } else {
            Style::new().on_light_red()
        };
        let cancel_para = Paragraph::new("Cancel").centered().style(cancel_style);
        frame.render_widget(cancel_para, cancel);
        cancel
    }
}

impl FailedState {
    fn render(&self, frame: &mut Frame, mouse_pos: (u16, u16)) -> (Rect, Option<Rect>) {
        const RETRY: &str = " Retry now ";
        let mouse_pos = Position::new(mouse_pos.0, mouse_pos.1);
        let area = frame.area();
        let [area, help_area] = area
            .try_layout(&Layout::vertical([
                Constraint::Fill(1),
                Constraint::Length(1),
            ]))
            .unwrap();
        let para = Paragraph::new(
            "Next: Tab, Previous: Shift+Tab, Exit: q, Select: Enter; (Can use mouse too!)",
        );
        frame.render_widget(para, help_area);
        let [mid] = area
            .try_layout(&Layout::horizontal([Constraint::Percentage(40)]).flex(Flex::Center))
            .unwrap();
        let [msg, retry_in_rect, _, buttons] = mid
            .try_layout(
                &Layout::vertical([
                    Constraint::Length(7),
                    Constraint::Length(3),
                    Constraint::Length(1),
                    Constraint::Length(1),
                ])
                .flex(Flex::Center),
            )
            .unwrap();
        let msg_para = Paragraph::new(self.msg.clone())
            .centered()
            .block(Block::bordered().title(" Error "))
            .wrap(Wrap { trim: false });
        if !self.permanent {
            let retry_in = (self.retry_at - Instant::now()).as_secs_f32();
            let retry_in = format!("Retrying in: {retry_in:.3}s");
            let retry_in_para = Paragraph::new(retry_in).block(Block::bordered()).centered();
            frame.render_widget(retry_in_para, retry_in_rect);
        }
        frame.render_widget(msg_para, msg);
        let (cancel_rect, retry_rect) = if self.permanent {
            let [cancel] = buttons
                .try_layout(
                    &Layout::horizontal([Constraint::Length(CANCEL.len() as u16)])
                        .flex(Flex::Center),
                )
                .unwrap();
            (cancel, None)
        } else {
            let [retry, cancel] = buttons
                .try_layout(
                    &Layout::horizontal([
                        Constraint::Length(RETRY.len() as u16),
                        Constraint::Length(CANCEL.len() as u16),
                    ])
                    .spacing(Spacing::Space(1))
                    .flex(Flex::Center),
                )
                .unwrap();
            (cancel, Some(retry))
        };
        let cancel_style = if cancel_rect.contains(mouse_pos) {
            Style::new().black().on_magenta()
        } else {
            Style::new().black().on_light_red()
        };

        let cancel_para = Paragraph::new(" Cancel ").centered().style(cancel_style);
        frame.render_widget(cancel_para, cancel_rect);
        if let Some(retry) = retry_rect {
            let style = if retry.contains(mouse_pos) {
                Style::new().black().on_green()
            } else {
                Style::new().black().on_gray()
            };
            let retry_para = Paragraph::new(" Retry now").centered().style(style);
            frame.render_widget(retry_para, retry);
        }
        (cancel_rect, retry_rect)
    }
}

impl RunningState {
    fn render(&mut self, frame: &mut Frame, mouse_pos: (u16, u16)) -> RunningBoundingBoxes {
        let area = frame.area();
        let [area, help_area] = area
            .try_layout(&Layout::vertical([
                Constraint::Fill(1),
                Constraint::Length(1),
            ]))
            .unwrap();
        let para = Paragraph::new(
            "Main Menu: q, Exit: Ctrl+C; (Some controls currently only available through mouse)",
        );
        frame.render_widget(para, help_area);
        let pos = Position::new(mouse_pos.0, mouse_pos.1);
        let [left, right] = area
            .try_layout(&Layout::horizontal([Constraint::Fill(1); 2]))
            .unwrap();
        let [ip, players] = left
            .try_layout(&Layout::vertical([
                Constraint::Length(3),
                Constraint::Fill(1),
            ]))
            .unwrap();
        let players_inner = players.inner(Margin::new(1, 1));
        let logs = right.inner(Margin::new(1, 1));
        let ip_style = if ip.contains(pos) {
            Style::new().light_green()
        } else {
            Style::new().cyan()
        };
        let ip_para = if self.copied {
            let mut text = Text::default();
            text.push_span(Span::raw(&*self.ip));
            text.push_span(Span::styled(" - Copied ", Style::new().cyan()));
            Paragraph::new(text)
        } else {
            Paragraph::new(&*self.ip)
        };
        let ip_para = ip_para.block(Block::bordered().title(" Server IP ").style(ip_style));
        frame.render_widget(ip_para, ip);
        let block = Block::bordered().title(" Players ");
        frame.render_widget(block, players);
        let block = Block::bordered().title(" Logs ");
        frame.render_widget(block, right);
        let mut rects = Vec::new();
        let mut players_rect = players_inner;
        while !players_rect.is_empty() {
            let r = Rect::new(players_rect.x, players_rect.y, players_rect.width, 1);
            players_rect.y += 1;
            players_rect.height -= 1;
            rects.push(r);
        }
        let mut disconnect_hitboxes = Vec::new();
        for (info, rect) in self.players.range(self.scroll_players..).zip(&rects) {
            const DISCONNECT: &str = " Disconnect ";
            let [name, addr, _, disconnect] = rect
                .try_layout(
                    &Layout::horizontal([
                        Constraint::Length(16),
                        Constraint::Length(21),
                        Constraint::Fill(1),
                        Constraint::Length(DISCONNECT.len() as u16),
                    ])
                    .spacing(Spacing::Space(1)),
                )
                .unwrap();
            disconnect_hitboxes.push((info.addr, disconnect));
            let name_str = "TEST";
            let name_para = Paragraph::new(name_str);
            let addr_para = Paragraph::new(info.addr.to_string());
            let disconnect_style = if disconnect.contains(pos) {
                Style::new().on_magenta()
            } else {
                Style::new().on_light_red()
            };
            let disconnect_para = Paragraph::new(DISCONNECT).style(disconnect_style);
            frame.render_widget(name_para, name);
            frame.render_widget(addr_para, addr);
            frame.render_widget(disconnect_para, disconnect);
        }
        rects.clear();
        let mut logs_rect = logs;
        while !logs_rect.is_empty() {
            let r = Rect::new(logs_rect.x, logs_rect.y, logs_rect.width, 1);
            logs_rect.y += 1;
            logs_rect.height -= 1;
            rects.push(r);
        }
        for ((timestamp, log), rect) in self.logs.range(self.scroll_logs..).zip(rects.iter().rev())
        {
            let [time_rect, log_rect] = rect
                .try_layout(
                    &Layout::horizontal([Constraint::Length(20), Constraint::Fill(1)])
                        .spacing(Spacing::Space(2)),
                )
                .unwrap();
            let time_para = Paragraph::new(timestamp.to_string());
            let log_para = Paragraph::new(&**log);
            frame.render_widget(time_para, time_rect);
            frame.render_widget(log_para, log_rect);
        }

        RunningBoundingBoxes {
            disconnects: disconnect_hitboxes,
            ip_box: ip,
            players_box: players_inner,
            logs_box: logs,
        }
    }
}
