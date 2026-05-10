mod module_bindings;
use module_bindings::*;

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseEventKind,
    },
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
        Wrap,
    },
    Terminal,
};
use spacetimedb_sdk::{DbContext, Identity, Table, TableWithPrimaryKey};
use std::env;
use std::io::{self};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

fn reducer_callback(state: &State, result: Result<Result<(), String>, impl std::fmt::Display>) {
    let err_msg = match result {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(e),
        Err(e) => Some(format!("Internal error: {e}")),
    };
    if let Some(msg) = err_msg {
        state.lock().unwrap().push(LogMessage::error(msg));
    }
}

// ─────────────────────────────────────────────
//  AUTOCOMPLETE
// ─────────────────────────────────────────────

const COMMANDS: &[&str] = &[
    "help",
    "whoami",
    "passwd",
    "logout",
    "lobby",
    "join",
    "leave",
    "games",
    "game",
    "move",
    "resign",
    "draw",
    "history",
    "pgn",
    "spectate",
    "unspectate",
    "spectators",
    "chat",
    "leaderboard",
    "login",
    "register",
    "quit",
    "exit",
];

fn autocomplete(input: &str) -> Option<String> {
    let word = input.split_whitespace().next().unwrap_or(input);
    if word.is_empty() || input.contains(' ') {
        return None;
    }
    let matches: Vec<&&str> = COMMANDS.iter().filter(|c| c.starts_with(word)).collect();
    if matches.len() == 1 {
        Some(matches[0].to_string())
    } else {
        None
    }
}

// ─────────────────────────────────────────────
//  LOG MESSAGES
// ─────────────────────────────────────────────

#[derive(Clone)]
enum LogKind {
    Info,
    Error,
    Chat,
    System,
}

#[derive(Clone)]
struct LogMessage {
    kind: LogKind,
    text: String,
}

impl LogMessage {
    fn info(text: impl Into<String>) -> Self {
        Self {
            kind: LogKind::Info,
            text: text.into(),
        }
    }
    fn error(text: impl Into<String>) -> Self {
        Self {
            kind: LogKind::Error,
            text: text.into(),
        }
    }
    fn chat(text: impl Into<String>) -> Self {
        Self {
            kind: LogKind::Chat,
            text: text.into(),
        }
    }
    fn system(text: impl Into<String>) -> Self {
        Self {
            kind: LogKind::System,
            text: text.into(),
        }
    }
}

// ─────────────────────────────────────────────
//  SHARED CLIENT STATE
// ─────────────────────────────────────────────

struct ClientState {
    my_identity: Option<Identity>,
    my_user_id: Option<u64>,
    my_username: Option<String>,
    active_game_id: Option<u64>,
    spectating_game_id: Option<u64>,
    authenticated: bool,
    // UI state
    log: Vec<LogMessage>,
    current_fen: Option<String>,
    current_game_white_uid: Option<u64>,
    status_line: String,
    input: String,
    input_cursor: usize,
    log_scroll: usize,
    follow_log: bool,
    // Password mode
    password_mode: bool,
    password_buffer: String,
    password_prompt: String,
}

impl Default for ClientState {
    fn default() -> Self {
        Self {
            my_identity: None,
            my_user_id: None,
            my_username: None,
            active_game_id: None,
            spectating_game_id: None,
            authenticated: false,
            log: Vec::new(),
            current_fen: None,
            current_game_white_uid: None,
            status_line: "No active game".to_string(),
            input: String::new(),
            input_cursor: 0,
            log_scroll: 0,
            follow_log: true,
            password_mode: false,
            password_buffer: String::new(),
            password_prompt: String::new(),
        }
    }
}

impl ClientState {
    fn push(&mut self, msg: LogMessage) {
        self.log.push(msg);

        if self.follow_log {
            self.log_scroll = self.log.len().saturating_sub(1);
        }
    }
}

type State = Arc<Mutex<ClientState>>;

// ─────────────────────────────────────────────
//  MAIN
// ─────────────────────────────────────────────

fn main() {
    let host = env::var("SPACETIMEDB_HOST").unwrap_or_else(|_| "http://localhost:3000".to_string());
    let db_name = env::var("SPACETIMEDB_DB_NAME").unwrap_or_else(|_| "spacetime-chess".to_string());

    // Setup terminal
    terminal::enable_raw_mode().expect("enable raw mode");
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture).expect("enter alternate screen");
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).expect("create terminal");

    let state: State = Arc::new(Mutex::new(ClientState::default()));
    let conn_holder: Arc<Mutex<Option<Arc<DbConnection>>>> = Arc::new(Mutex::new(None));

    let conn_holder_clone = conn_holder.clone();
    let state_for_connect = state.clone();

    let conn = Arc::new(
        DbConnection::builder()
            .with_database_name(db_name)
            .with_uri(host)
            .on_connect(move |ctx, identity, _token| {
                {
                    let mut st = state_for_connect.lock().unwrap();
                    st.my_identity = Some(identity);
                    st.authenticated = false;
                    st.push(LogMessage::system("✓ Connected to SpacetimeDB"));
                    st.push(LogMessage::system(
                        "Use 'login <user>' or 'register <user>'",
                    ));
                }

                ctx.subscription_builder()
                    .on_error(|_ctx, e| eprintln!("[subscription error] {e}"))
                    .add_query(|q| q.from.user())
                    .add_query(|q| q.from.game())
                    .add_query(|q| q.from.lobby_entry())
                    .add_query(|q| q.from.move_record())
                    .add_query(|q| q.from.spectator())
                    .add_query(|q| q.from.chat_message())
                    .add_query(|q| q.from.session())
                    .subscribe();

                // AUTH EVENTS
                ctx.db().session().on_insert({
                    let state = state_for_connect.clone();
                    let conn_holder = conn_holder_clone.clone();
                    move |_ctx, session| {
                        let mut st = state.lock().unwrap();
                        if Some(session.identity) == st.my_identity {
                            let conn = conn_holder.lock().unwrap();
                            let conn = conn.as_ref().unwrap();
                            let username = conn
                                .db()
                                .user()
                                .user_id()
                                .find(&session.user_id)
                                .map(|u| u.username)
                                .unwrap_or_else(|| format!("user#{}", session.user_id));
                            st.my_user_id = Some(session.user_id);
                            st.my_username = Some(username.clone());
                            st.authenticated = true;
                            st.push(LogMessage::system(format!(
                                "✓ Logged in as '{}'. Type 'help' for commands.",
                                username
                            )));
                        }
                    }
                });

                ctx.db().session().on_delete({
                    let state = state_for_connect.clone();
                    move |_ctx, session| {
                        let mut st = state.lock().unwrap();
                        if Some(session.identity) == st.my_identity {
                            st.my_user_id = None;
                            st.my_username = None;
                            st.authenticated = false;
                            st.push(LogMessage::system("Logged out."));
                        }
                    }
                });

                // GAME EVENTS
                ctx.db().game().on_insert({
                    let state = state_for_connect.clone();
                    move |_ctx, game| {
                        let mut st = state.lock().unwrap();
                        if let Some(uid) = st.my_user_id {
                            if game.white_user_id == uid || game.black_user_id == uid {
                                let my_color = if game.white_user_id == uid {
                                    "White ♔"
                                } else {
                                    "Black ♚"
                                };
                                st.active_game_id = Some(game.id);
                                st.current_fen = Some(game.fen.clone());
                                st.current_game_white_uid = Some(game.white_user_id);
                                st.status_line = format!(
                                    "🎮 Game #{} started! You are {}  ({} vs {})",
                                    game.id, my_color, game.white_username, game.black_username
                                );
                            }
                        }
                    }
                });

                ctx.db().game().on_update({
                    let state = state_for_connect.clone();
                    move |_ctx, _old, g| {
                        let mut st = state.lock().unwrap();
                        let uid = st.my_user_id;
                        let watching =
                            st.active_game_id == Some(g.id) || st.spectating_game_id == Some(g.id);
                        let is_player =
                            uid.map_or(false, |u| u == g.white_user_id || u == g.black_user_id);

                        if watching || is_player {
                            st.current_fen = Some(g.fen.clone());
                            st.current_game_white_uid = Some(g.white_user_id);
                            st.status_line = match g.status {
                                GameStatus::InProgress => {
                                    let (name, color) = if g.turn == PieceColor::White {
                                        (&g.white_username, "White")
                                    } else {
                                        (&g.black_username, "Black")
                                    };
                                    format!("Turn: {} ({})", name, color)
                                }
                                GameStatus::Checkmate => "♛ Checkmate!".to_string(),
                                GameStatus::Stalemate => "½ Stalemate — draw".to_string(),
                                GameStatus::Draw => "½ Draw agreed".to_string(),
                                GameStatus::Resigned => "🏳 A player resigned".to_string(),
                                _ => st.status_line.clone(),
                            };
                        }
                    }
                });

                ctx.db().chat_message().on_insert({
                    let state = state_for_connect.clone();
                    move |_ctx, msg| {
                        let mut st = state.lock().unwrap();
                        if st.my_user_id.is_none() {
                            return;
                        }
                        let relevant = msg.game_id == 0
                            || st.active_game_id == Some(msg.game_id)
                            || st.spectating_game_id == Some(msg.game_id);
                        if relevant {
                            let label = if msg.game_id == 0 {
                                "lobby".to_string()
                            } else {
                                format!("game #{}", msg.game_id)
                            };
                            st.push(LogMessage::chat(format!(
                                "[{}] {}: {}",
                                label, msg.sender_name, msg.text
                            )));
                        }
                    }
                });

                let conn_arc = conn_holder_clone.lock().unwrap().as_ref().unwrap().clone();
                let state = state_for_connect.clone();
                std::thread::spawn(move || repl_loop(&conn_arc, &state));
            })
            .on_connect_error(|_ctx, e| {
                eprintln!("Connection error: {:?}", e);
                std::process::exit(1);
            })
            .build()
            .expect("Failed to connect"),
    );

    *conn_holder.lock().unwrap() = Some(conn.clone());
    conn.run_threaded();

    // ── Render loop ───────────────────────────────────────────────────────────
    loop {
        {
            let st = state.lock().unwrap();
            terminal.draw(|f| draw(f, &st)).ok();
        }

        if event::poll(Duration::from_millis(50)).unwrap_or(false) {
            if let Ok(ev) = event::read() {
                let mut st = state.lock().unwrap();
                match ev {
                    Event::Key(key) => {
                        if key.kind != KeyEventKind::Press {
                            continue;
                        }

                        if st.password_mode {
                            match key.code {
                                KeyCode::Char(c)
                                    if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                                {
                                    st.password_buffer.push(c);
                                }

                                KeyCode::Backspace => {
                                    st.password_buffer.pop();
                                }

                                KeyCode::Enter => {
                                    let pw = st.password_buffer.clone();

                                    st.password_mode = false;
                                    st.password_buffer.clear();

                                    if let Some(tx) = PASSWORD_TX.lock().unwrap().take() {
                                        let _ = tx.send(pw);
                                    }
                                }

                                KeyCode::Esc => {
                                    st.password_mode = false;
                                    st.password_buffer.clear();

                                    if let Some(tx) = PASSWORD_TX.lock().unwrap().take() {
                                        let _ = tx.send(String::new());
                                    }
                                }

                                _ => {}
                            }

                            continue;
                        }

                        match key.code {
                            // Submit
                            KeyCode::Enter => {
                                let input = st.input.trim().to_string();
                                st.input.clear();
                                st.input_cursor = 0;
                                if !input.is_empty() {
                                    st.push(LogMessage::info(format!("> {}", input)));
                                    drop(st);
                                    // Commands that need immediate handling in the render thread
                                    handle_immediate(input);
                                }
                            }

                            // Tab autocomplete
                            KeyCode::Tab => {
                                let input = st.input.clone();
                                if let Some(completed) = autocomplete(&input) {
                                    st.input = completed.clone() + " ";
                                    st.input_cursor = st.input.len();
                                } else {
                                    // Show candidates
                                    let word = input.split_whitespace().next().unwrap_or("");
                                    let candidates: Vec<String> = COMMANDS
                                        .iter()
                                        .filter(|c| c.starts_with(word))
                                        .map(|c| c.to_string())
                                        .collect();
                                    if !candidates.is_empty() {
                                        st.push(LogMessage::system(candidates.join("  ")));
                                    }
                                }
                            }

                            // Typing
                            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                                let cursor = st.input_cursor;
                                st.input.insert(cursor, c);
                                st.input_cursor += 1;
                            }

                            // Ctrl-C / Ctrl-D
                            KeyCode::Char('c') | KeyCode::Char('d')
                                if key.modifiers.contains(KeyModifiers::CONTROL) =>
                            {
                                drop(st);
                                cleanup_terminal();
                                std::process::exit(0);
                            }

                            // Backspace
                            KeyCode::Backspace => {
                                if st.input_cursor > 0 {
                                    st.input_cursor -= 1;
                                    let cur = st.input_cursor;
                                    st.input.remove(cur);
                                }
                            }

                            // Delete
                            KeyCode::Delete => {
                                let cur = st.input_cursor;
                                if cur < st.input.len() {
                                    st.input.remove(cur);
                                }
                            }

                            // Cursor movement
                            KeyCode::Left => {
                                if st.input_cursor > 0 {
                                    st.input_cursor -= 1;
                                }
                            }
                            KeyCode::Right => {
                                if st.input_cursor < st.input.len() {
                                    st.input_cursor += 1;
                                }
                            }
                            KeyCode::Home => st.input_cursor = 0,
                            KeyCode::End => st.input_cursor = st.input.len(),

                            // Log scrolling
                            KeyCode::PageUp => {
                                st.log_scroll = st.log_scroll.saturating_sub(5);
                            }
                            KeyCode::PageDown => {
                                st.log_scroll =
                                    (st.log_scroll + 5).min(st.log.len().saturating_sub(1));
                            }

                            _ => {}
                        }
                    }
                    Event::Mouse(mouse) => match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            st.log_scroll = st.log_scroll.saturating_sub(5);
                        }
                        MouseEventKind::ScrollDown => {
                            st.log_scroll = (st.log_scroll + 5).min(st.log.len().saturating_sub(1));
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }

            if let Ok(Event::Key(key)) = event::read() {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let mut st = state.lock().unwrap();
                if st.password_mode {
                    match key.code {
                        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                            st.password_buffer.push(c);
                        }

                        KeyCode::Backspace => {
                            st.password_buffer.pop();
                        }

                        KeyCode::Enter => {
                            let pw = st.password_buffer.clone();

                            st.password_mode = false;
                            st.password_buffer.clear();

                            if let Some(tx) = PASSWORD_TX.lock().unwrap().take() {
                                let _ = tx.send(pw);
                            }
                        }

                        KeyCode::Esc => {
                            st.password_mode = false;
                            st.password_buffer.clear();

                            if let Some(tx) = PASSWORD_TX.lock().unwrap().take() {
                                let _ = tx.send(String::new());
                            }
                        }

                        _ => {}
                    }

                    continue;
                }
                match key.code {
                    // Submit
                    KeyCode::Enter => {
                        let input = st.input.trim().to_string();
                        st.input.clear();
                        st.input_cursor = 0;
                        if !input.is_empty() {
                            st.push(LogMessage::info(format!("> {}", input)));
                            drop(st);
                            // Commands that need immediate handling in the render thread
                            handle_immediate(input);
                        }
                    }

                    // Tab autocomplete
                    KeyCode::Tab => {
                        let input = st.input.clone();
                        if let Some(completed) = autocomplete(&input) {
                            st.input = completed.clone() + " ";
                            st.input_cursor = st.input.len();
                        } else {
                            // Show candidates
                            let word = input.split_whitespace().next().unwrap_or("");
                            let candidates: Vec<String> = COMMANDS
                                .iter()
                                .filter(|c| c.starts_with(word))
                                .map(|c| c.to_string())
                                .collect();
                            if !candidates.is_empty() {
                                st.push(LogMessage::system(candidates.join("  ")));
                            }
                        }
                    }

                    // Typing
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        let cursor = st.input_cursor;
                        st.input.insert(cursor, c);
                        st.input_cursor += 1;
                    }

                    // Ctrl-C / Ctrl-D
                    KeyCode::Char('c') | KeyCode::Char('d')
                        if key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        drop(st);
                        cleanup_terminal();
                        std::process::exit(0);
                    }

                    // Backspace
                    KeyCode::Backspace => {
                        if st.input_cursor > 0 {
                            st.input_cursor -= 1;
                            let cur = st.input_cursor;
                            st.input.remove(cur);
                        }
                    }

                    // Delete
                    KeyCode::Delete => {
                        let cur = st.input_cursor;
                        if cur < st.input.len() {
                            st.input.remove(cur);
                        }
                    }

                    // Cursor movement
                    KeyCode::Left => {
                        if st.input_cursor > 0 {
                            st.input_cursor -= 1;
                        }
                    }
                    KeyCode::Right => {
                        if st.input_cursor < st.input.len() {
                            st.input_cursor += 1;
                        }
                    }
                    KeyCode::Home => st.input_cursor = 0,
                    KeyCode::End => st.input_cursor = st.input.len(),

                    // Log scrolling
                    KeyCode::PageUp => {
                        st.log_scroll = st.log_scroll.saturating_sub(5);
                    }
                    KeyCode::PageDown => {
                        st.log_scroll = (st.log_scroll + 5).min(st.log.len().saturating_sub(1));
                    }

                    _ => {}
                }
            }
        }
    }
}

fn cleanup_terminal() {
    let mut stdout = io::stdout();
    execute!(stdout, LeaveAlternateScreen, DisableMouseCapture).ok();
    terminal::disable_raw_mode().ok();
}

// Quit is handled in the render thread directly since it just exits
fn handle_immediate(input: String) {
    let lower = input.trim().to_lowercase();
    if lower == "quit" || lower == "exit" {
        cleanup_terminal();
        std::process::exit(0);
    }
    // All other commands are handled in repl_loop via the shared command channel
    // They are pushed via COMMAND_TX below
    if let Some(tx) = COMMAND_TX.lock().unwrap().as_ref() {
        let _ = tx.send(input);
    }
}

// Global command channel: render thread -> repl thread
static COMMAND_TX: std::sync::LazyLock<Mutex<Option<mpsc::Sender<String>>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));

// ─────────────────────────────────────────────
//  DRAW
// ─────────────────────────────────────────────

fn draw(f: &mut ratatui::Frame, st: &ClientState) {
    let full = f.area();

    // Split into main area + input bar
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(3)])
        .split(full);

    let main_area = vertical[0];
    let input_area = vertical[1];

    // Split main into board (left) + log (right)
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(50), Constraint::Min(0)])
        .split(main_area);

    let board_area = horizontal[0];
    let log_area = horizontal[1];

    draw_board(f, st, board_area);
    draw_log(f, st, log_area);
    draw_input(f, st, input_area);
}

fn draw_board(f: &mut ratatui::Frame, st: &ClientState, area: Rect) {
    // Split into board + status line
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(3)])
        .split(area);

    let board_area = layout[0];
    let status_area = layout[1];

    // BOARD BLOCK
    let block = Block::default().title(" Board ").borders(Borders::ALL);

    let inner = block.inner(board_area);

    f.render_widget(block, board_area);

    // STATUS BLOCK
    let status = Paragraph::new(st.status_line.clone())
        .block(Block::default().title(" Status ").borders(Borders::ALL));

    f.render_widget(status, status_area);

    // NO GAME
    let Some(ref fen) = st.current_fen else {
        let placeholder = Paragraph::new("No active game.\n\nUse 'games'\nthen\n'game <id>'.")
            .wrap(Wrap { trim: false });

        f.render_widget(placeholder, inner);
        return;
    };

    // NORMAL BOARD RENDERING
    let white_uid = st.current_game_white_uid;

    let white_view = st
        .my_user_id
        .and_then(|uid| white_uid.map(|w| uid == w))
        .unwrap_or(true);

    let board_part = fen.split(' ').next().unwrap_or(fen.as_str());
    let mut squares: [[char; 8]; 8] = [['.'; 8]; 8];

    for (ri, row) in board_part.split('/').enumerate() {
        let rank = 7 - ri;
        let mut file = 0usize;

        for ch in row.chars() {
            if let Some(n) = ch.to_digit(10) {
                file += n as usize;
            } else {
                squares[rank][file] = ch;
                file += 1;
            }
        }
    }

    let ranks: Vec<u8> = if white_view {
        (0..8).rev().collect()
    } else {
        (0..8).collect()
    };
    let files: Vec<u8> = if white_view {
        (0..8).collect()
    } else {
        (0..8).rev().collect()
    };
    let mut lines: Vec<Line> = Vec::new();
    for rank in &ranks {
        let mut spans = vec![Span::raw(format!(" {} ", rank + 1))];
        for file in &files {
            let ch = squares[*rank as usize][*file as usize];

            let piece = piece_unicode(ch);
            let is_black_piece = ch.is_lowercase() && ch != '.';

            let fg = if is_black_piece {
                Color::Rgb(30, 30, 30)
            } else {
                Color::Rgb(255, 255, 255)
            };

            let bg = if (rank + file) % 2 == 0 {
                Color::Rgb(110, 78, 55)
            } else {
                Color::Rgb(181, 136, 99)
            };
            spans.push(Span::styled(
                format!(" {} ", piece),
                Style::default().fg(fg).bg(bg),
            ));
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::from(""));
    let file_labels = if white_view {
        "    a  b  c  d  e  f  g  h"
    } else {
        "    h  g  f  e  d  c  b  a"
    };
    lines.push(Line::from(file_labels));

    let board_widget = Paragraph::new(lines);
    f.render_widget(board_widget, inner);
}

fn draw_log(f: &mut ratatui::Frame, st: &ClientState, area: Rect) {
    let title = if let Some(name) = &st.my_username {
        format!(" Log — {} ", name)
    } else {
        " Log ".to_string()
    };

    let block = Block::default().title(title).borders(Borders::ALL);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let visible_height = inner.height as usize;
    let total = st.log.len();
    let max_scroll = total.saturating_sub(visible_height);
    let scroll = st.log_scroll.min(max_scroll);
    let end = (scroll + visible_height).min(total);

    let items: Vec<ListItem> = st.log[scroll..end]
        .iter()
        .map(|msg| {
            let style = match msg.kind {
                LogKind::Error => Style::default()
                    .fg(ratatui::style::Color::Red)
                    .add_modifier(Modifier::BOLD),
                LogKind::Chat => Style::default().add_modifier(Modifier::ITALIC),
                LogKind::System => Style::default().add_modifier(Modifier::DIM),
                LogKind::Info => Style::default(),
            };
            ListItem::new(Line::from(Span::styled(msg.text.clone(), style)))
        })
        .collect();
    let list = List::new(items);
    f.render_widget(list, inner);

    // Scrollbar
    let scrollbar = Scrollbar::default().orientation(ScrollbarOrientation::VerticalRight);
    let mut scrollbar_state = ScrollbarState::new(total / 2)
        .viewport_content_length(visible_height)
        .position(scroll);
    f.render_stateful_widget(scrollbar, area, &mut scrollbar_state);
}

fn draw_input(f: &mut ratatui::Frame, st: &ClientState, area: Rect) {
    let prompt = if st.password_mode {
        format!("{}: ", st.password_prompt)
    } else if st.authenticated {
        format!("{}> ", st.my_username.as_deref().unwrap_or(""))
    } else {
        "auth> ".to_string()
    };

    // What is actually displayed
    let display_input = if st.password_mode {
        "•".repeat(st.password_buffer.len())
    } else {
        st.input.clone()
    };

    // Cursor position source
    let cursor_pos = if st.password_mode {
        display_input.len()
    } else {
        st.input_cursor
    };

    // Autocomplete hint only for normal mode
    let hint = if !st.password_mode && !st.input.is_empty() && !st.input.contains(' ') {
        let word = &st.input;

        let matches: Vec<&&str> = COMMANDS
            .iter()
            .filter(|c| c.starts_with(word.as_str()))
            .collect();

        if matches.len() == 1 && *matches[0] != word.as_str() {
            format!("{}", &matches[0][word.len()..])
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    let before_cursor = &display_input[..cursor_pos];

    let cursor_char = display_input[cursor_pos..]
        .chars()
        .next()
        .map(|c| c.to_string())
        .unwrap_or(" ".to_string());

    let after_cursor = if cursor_pos < display_input.len() {
        &display_input[cursor_pos + cursor_char.len()..]
    } else {
        ""
    };

    let spans = vec![
        Span::raw(format!(" {}{}", prompt, before_cursor)),
        Span::styled(
            cursor_char,
            Style::default().add_modifier(Modifier::REVERSED),
        ),
        Span::raw(after_cursor.to_string()),
        Span::styled(hint, Style::default().add_modifier(Modifier::DIM)),
    ];

    let title = if st.password_mode {
        " Password Input "
    } else {
        " Input "
    };

    let input_widget = Paragraph::new(Line::from(spans))
        .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(input_widget, area);
}

// ─────────────────────────────────────────────
//  REPL LOOP  (runs in its own thread)
// ─────────────────────────────────────────────

fn repl_loop(conn: &Arc<DbConnection>, state: &State) {
    let (tx, rx) = mpsc::channel::<String>();
    *COMMAND_TX.lock().unwrap() = Some(tx);

    loop {
        let Ok(line) = rx.recv() else { break };
        let parts: Vec<&str> = line.trim().split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }

        match parts[0] {
            "help" => {
                let mut st = state.lock().unwrap();
                for line in HELP_LINES {
                    st.push(LogMessage::system(*line));
                }
            }

            // ── Account ───────────────────────────────────────────────────
            "login" => {
                let username = parts.get(1).unwrap_or(&"").to_string();
                if username.is_empty() {
                    state
                        .lock()
                        .unwrap()
                        .push(LogMessage::error("Usage: login <username>"));
                    continue;
                }
                // Check if user exists in local cache before prompting for password
                if conn.db().user().username().find(&username).is_none() {
                    state
                        .lock()
                        .unwrap()
                        .push(LogMessage::error("Unknown username."));
                    continue;
                }
                let password = prompt_password(state, "Password");
                let state_cb = Arc::clone(&state);
                let _ = conn
                    .reducers()
                    .login_then(username, password, move |_ctx, result| {
                        reducer_callback(&state_cb, result);
                    });
            }

            "register" => {
                let username = parts.get(1).unwrap_or(&"").to_string();
                if username.is_empty() {
                    state
                        .lock()
                        .unwrap()
                        .push(LogMessage::error("Usage: register <username>"));
                    continue;
                }
                let password = prompt_password(state, "Password");
                let confirm = prompt_password(state, "Confirm password");
                if password != confirm {
                    state
                        .lock()
                        .unwrap()
                        .push(LogMessage::error("Passwords do not match"));
                    continue;
                }
                let state_cb = Arc::clone(&state);
                let conn_cb = Arc::clone(&conn);
                let _ = conn.reducers().register_then(
                    username.clone(),
                    password.clone(),
                    move |_ctx, result| match result {
                        Ok(Ok(())) => {
                            state_cb
                                .lock()
                                .unwrap()
                                .push(LogMessage::system("Registered. Logging in..."));
                            let state_cb2 = Arc::clone(&state_cb);
                            let _ = conn_cb.reducers().login_then(
                                username,
                                password,
                                move |_ctx, result| {
                                    reducer_callback(&state_cb2, result);
                                },
                            );
                        }
                        Ok(Err(e)) => state_cb.lock().unwrap().push(LogMessage::error(e)),
                        Err(e) => state_cb
                            .lock()
                            .unwrap()
                            .push(LogMessage::error(format!("Internal error: {e}"))),
                    },
                );
            }

            "logout" => {
                let state_cb = Arc::clone(&state);
                let _ = conn.reducers().logout_then(move |_ctx, result| {
                    reducer_callback(&state_cb, result);
                });
                let mut st = state.lock().unwrap();
                st.my_user_id = None;
                st.my_username = None;
                st.authenticated = false;
                st.active_game_id = None;
                st.spectating_game_id = None;
                st.current_fen = None;
                st.status_line = "No active game".to_string();
            }

            "passwd" => {
                let current = prompt_password(state, "Current password");
                let new_pw = prompt_password(state, "New password (min 8 chars)");
                let confirm = prompt_password(state, "Confirm new password");
                if new_pw != confirm {
                    state
                        .lock()
                        .unwrap()
                        .push(LogMessage::error("Passwords do not match."));
                } else {
                    let state_cb = Arc::clone(&state);
                    let _ = conn.reducers().change_password_then(
                        current,
                        new_pw,
                        move |_ctx, result| match result {
                            Ok(Ok(())) => state_cb
                                .lock()
                                .unwrap()
                                .push(LogMessage::system("✓ Password changed.")),
                            Ok(Err(e)) => state_cb.lock().unwrap().push(LogMessage::error(e)),
                            Err(e) => state_cb
                                .lock()
                                .unwrap()
                                .push(LogMessage::error(format!("Internal error: {e}"))),
                        },
                    );
                }
            }

            "whoami" => {
                let st = state.lock().unwrap();
                if let (Some(uid), Some(uname)) = (st.my_user_id, st.my_username.clone()) {
                    drop(st);
                    if let Some(user) = conn.db().user().user_id().find(&uid) {
                        state.lock().unwrap().push(LogMessage::info(format!(
                            "{} (id={})  W:{} L:{} D:{}",
                            user.username, user.user_id, user.wins, user.losses, user.draws
                        )));
                    } else {
                        state
                            .lock()
                            .unwrap()
                            .push(LogMessage::info(format!("{} (id={})", uname, uid)));
                    }
                }
            }

            // ── Lobby ──────────────────────────────────────────────────────
            "lobby" => {
                let entries: Vec<_> = conn
                    .db()
                    .lobby_entry()
                    .iter()
                    .filter(|e| e.status == LobbyStatus::Open)
                    .collect();
                let mut st = state.lock().unwrap();
                st.push(LogMessage::system("── Matchmaking Lobby ──"));
                if entries.is_empty() {
                    st.push(LogMessage::info("  (lobby is empty)"));
                }
                for e in &entries {
                    st.push(LogMessage::info(format!(
                        "  [{}] {} — waiting",
                        e.id, e.username
                    )));
                }
            }

            "join" => {
                let state_cb = Arc::clone(&state);
                let _ = conn
                    .reducers()
                    .join_lobby_then(move |_ctx, result| match result {
                        Ok(Ok(())) => state_cb
                            .lock()
                            .unwrap()
                            .push(LogMessage::system("Joined matchmaking lobby…")),
                        Ok(Err(e)) => state_cb.lock().unwrap().push(LogMessage::error(e)),
                        Err(e) => state_cb
                            .lock()
                            .unwrap()
                            .push(LogMessage::error(format!("Internal error: {e}"))),
                    });
            }

            "leave" => {
                let state_cb = Arc::clone(&state);
                let _ = conn
                    .reducers()
                    .leave_lobby_then(move |_ctx, result| match result {
                        Ok(Ok(())) => state_cb
                            .lock()
                            .unwrap()
                            .push(LogMessage::system("Left lobby.")),
                        Ok(Err(e)) => state_cb.lock().unwrap().push(LogMessage::error(e)),
                        Err(e) => state_cb
                            .lock()
                            .unwrap()
                            .push(LogMessage::error(format!("Internal error: {e}"))),
                    });
            }

            // ── Games ──────────────────────────────────────────────────────
            "games" => {
                let games: Vec<_> = conn
                    .db()
                    .game()
                    .iter()
                    .filter(|g| {
                        g.status == GameStatus::InProgress
                            || g.status == GameStatus::WaitingForOpponent
                    })
                    .collect();
                let mut st = state.lock().unwrap();
                st.push(LogMessage::system("── Active Games ──"));
                if games.is_empty() {
                    st.push(LogMessage::info("  (no active games)"));
                }
                for g in &games {
                    st.push(LogMessage::info(format!(
                        "  #{} — {} (W) vs {} (B)  [turn: {:?}]",
                        g.id, g.white_username, g.black_username, g.turn
                    )));
                }
            }

            "game" => {
                if parts.len() < 2 {
                    state
                        .lock()
                        .unwrap()
                        .push(LogMessage::error("Usage: game <id>"));
                } else if let Ok(id) = parts[1].parse::<u64>() {
                    if let Some(game) = conn.db().game().iter().find(|g| g.id == id) {
                        let mut st = state.lock().unwrap();
                        st.active_game_id = Some(id);
                        st.current_fen = Some(game.fen.clone());
                        st.current_game_white_uid = Some(game.white_user_id);
                        st.push(LogMessage::system(format!("Viewing game #{}", id)));
                        st.status_line = match game.status {
                            GameStatus::InProgress => {
                                let (name, color) = if game.turn == PieceColor::White {
                                    (&game.white_username, "White")
                                } else {
                                    (&game.black_username, "Black")
                                };
                                format!("Turn: {} ({})", name, color)
                            }
                            GameStatus::Checkmate => "♛ Checkmate!".to_string(),
                            GameStatus::Stalemate => "½ Stalemate — draw".to_string(),
                            GameStatus::Draw => "½ Draw agreed".to_string(),
                            GameStatus::Resigned => "🏳 A player resigned".to_string(),
                            _ => st.status_line.clone(),
                        };
                    } else {
                        state
                            .lock()
                            .unwrap()
                            .push(LogMessage::error(format!("Game {} not found.", id)));
                    }
                } else {
                    state
                        .lock()
                        .unwrap()
                        .push(LogMessage::error("Invalid game id."));
                }
            }

            // ── Move ───────────────────────────────────────────────────────
            "move" => {
                if parts.len() < 3 {
                    state.lock().unwrap().push(LogMessage::error(
                        "Usage: move <from> <to> [promo]  e.g. move e2 e4",
                    ));
                    continue;
                }
                let gid = state.lock().unwrap().active_game_id;
                let Some(game_id) = gid else {
                    state
                        .lock()
                        .unwrap()
                        .push(LogMessage::error("No active game."));
                    continue;
                };
                match (parse_square(parts[1]), parse_square(parts[2])) {
                    (Ok((ff, fr)), Ok((tf, tr))) => {
                        let promo = parts.get(3).map(|s| s.to_string());
                        let state_cb = Arc::clone(&state);
                        let _ = conn.reducers().make_move_then(
                            game_id,
                            ff,
                            fr,
                            tf,
                            tr,
                            promo,
                            move |_ctx, result| {
                                reducer_callback(&state_cb, result);
                            },
                        );
                    }
                    (Err(e), _) => state
                        .lock()
                        .unwrap()
                        .push(LogMessage::error(format!("Bad source square: {e}"))),
                    (_, Err(e)) => state
                        .lock()
                        .unwrap()
                        .push(LogMessage::error(format!("Bad target square: {e}"))),
                }
            }

            // ── Resign / Draw ──────────────────────────────────────────────
            "resign" => {
                let gid = state.lock().unwrap().active_game_id;
                match gid {
                    Some(id) => {
                        let state_cb = Arc::clone(&state);
                        let _ = conn
                            .reducers()
                            .resign_then(id, move |_ctx, result| match result {
                                Ok(Ok(())) => state_cb
                                    .lock()
                                    .unwrap()
                                    .push(LogMessage::system("You resigned.")),
                                Ok(Err(e)) => state_cb.lock().unwrap().push(LogMessage::error(e)),
                                Err(e) => state_cb
                                    .lock()
                                    .unwrap()
                                    .push(LogMessage::error(format!("Internal error: {e}"))),
                            });
                    }
                    None => state
                        .lock()
                        .unwrap()
                        .push(LogMessage::error("No active game.")),
                }
            }

            "draw" => {
                let gid = state.lock().unwrap().active_game_id;
                match gid {
                    Some(id) => {
                        let state_cb = Arc::clone(&state);
                        let _ =
                            conn.reducers()
                                .offer_draw_then(id, move |_ctx, result| match result {
                                    Ok(Ok(())) => state_cb
                                        .lock()
                                        .unwrap()
                                        .push(LogMessage::system("Draw offer sent (or accepted).")),
                                    Ok(Err(e)) => {
                                        state_cb.lock().unwrap().push(LogMessage::error(e))
                                    }
                                    Err(e) => state_cb
                                        .lock()
                                        .unwrap()
                                        .push(LogMessage::error(format!("Internal error: {e}"))),
                                });
                    }
                    None => state
                        .lock()
                        .unwrap()
                        .push(LogMessage::error("No active game.")),
                }
            }

            // ── History / PGN ──────────────────────────────────────────────
            "history" => {
                let gid = resolve_game_id(&parts, &state);
                match gid {
                    Some(id) => {
                        let mut moves: Vec<MoveRecord> = conn
                            .db()
                            .move_record()
                            .iter()
                            .filter(|m| m.game_id == id)
                            .collect();
                        moves.sort_by_key(|m| m.ply);
                        let mut st = state.lock().unwrap();
                        if moves.is_empty() {
                            st.push(LogMessage::info(format!("No moves for game #{}", id)));
                        } else {
                            st.push(LogMessage::system(format!(
                                "── Move History — Game #{} ──",
                                id
                            )));
                            for m in &moves {
                                let num = (m.ply + 1) / 2;
                                let side = if m.piece_color == PieceColor::White {
                                    "W"
                                } else {
                                    "B"
                                };
                                let cap = if m.captured.is_some() { "x" } else { "-" };
                                st.push(LogMessage::info(format!(
                                    "  {:>3}. [{}] {}{} {} {}{}  {}",
                                    num,
                                    side,
                                    (b'a' + m.from_file) as char,
                                    m.from_rank + 1,
                                    cap,
                                    (b'a' + m.to_file) as char,
                                    m.to_rank + 1,
                                    m.san
                                )));
                            }
                        }
                    }
                    None => state
                        .lock()
                        .unwrap()
                        .push(LogMessage::error("Usage: history [game_id]")),
                }
            }

            "pgn" => {
                let gid = resolve_game_id(&parts, &state);
                match gid {
                    Some(id) => {
                        let game = conn.db().game().iter().find(|g| g.id == id);
                        match game {
                            None => state
                                .lock()
                                .unwrap()
                                .push(LogMessage::error(format!("Game #{} not found", id))),
                            Some(game) => {
                                let result_str = match game.status {
                                    GameStatus::Checkmate | GameStatus::Resigned => {
                                        match game.winner_user_id {
                                            Some(w) if w == game.white_user_id => "1-0",
                                            Some(_) => "0-1",
                                            None => "*",
                                        }
                                    }
                                    GameStatus::Draw | GameStatus::Stalemate => "1/2-1/2",
                                    _ => "*",
                                };
                                let mut st = state.lock().unwrap();
                                st.push(LogMessage::system(format!("── PGN — Game #{} ──", id)));
                                st.push(LogMessage::info(format!(
                                    "[White \"{}\"]",
                                    game.white_username
                                )));
                                st.push(LogMessage::info(format!(
                                    "[Black \"{}\"]",
                                    game.black_username
                                )));
                                st.push(LogMessage::info(format!("[Result \"{}\"]", result_str)));
                                drop(st);

                                let mut moves: Vec<MoveRecord> = conn
                                    .db()
                                    .move_record()
                                    .iter()
                                    .filter(|m| m.game_id == id)
                                    .collect();
                                moves.sort_by_key(|m| m.ply);
                                let mut pgn = String::new();
                                for m in &moves {
                                    if m.ply % 2 == 1 {
                                        pgn.push_str(&format!("{}. ", (m.ply + 1) / 2 + 1));
                                    }
                                    pgn.push_str(&m.san);
                                    pgn.push(' ');
                                }
                                pgn.push_str(result_str);
                                state.lock().unwrap().push(LogMessage::info(pgn));
                            }
                        }
                    }
                    None => state
                        .lock()
                        .unwrap()
                        .push(LogMessage::error("Usage: pgn [game_id]")),
                }
            }

            // ── Spectate ───────────────────────────────────────────────────
            "spectate" => {
                if parts.len() < 2 {
                    state
                        .lock()
                        .unwrap()
                        .push(LogMessage::error("Usage: spectate <game_id>"));
                } else if let Ok(id) = parts[1].parse::<u64>() {
                    let state_cb = Arc::clone(&state);
                    let _ =
                        conn.reducers()
                            .spectate_game_then(id, move |_ctx, result| match result {
                                Ok(Ok(())) => {
                                    let mut st = state_cb.lock().unwrap();
                                    st.spectating_game_id = Some(id);
                                    st.push(LogMessage::system(format!(
                                        "Now spectating game #{}",
                                        id
                                    )));
                                }
                                Ok(Err(e)) => state_cb.lock().unwrap().push(LogMessage::error(e)),
                                Err(e) => state_cb
                                    .lock()
                                    .unwrap()
                                    .push(LogMessage::error(format!("Internal error: {e}"))),
                            });
                } else {
                    state
                        .lock()
                        .unwrap()
                        .push(LogMessage::error("Invalid game id."));
                }
            }

            "unspectate" => {
                let gid = state.lock().unwrap().spectating_game_id;
                match gid {
                    Some(id) => {
                        let state_cb = Arc::clone(&state);
                        let _ = conn
                            .reducers()
                            .leave_spectate_then(id, move |_ctx, result| match result {
                                Ok(Ok(())) => {
                                    let mut st = state_cb.lock().unwrap();
                                    st.spectating_game_id = None;
                                    st.push(LogMessage::system("Stopped spectating."));
                                }
                                Ok(Err(e)) => state_cb.lock().unwrap().push(LogMessage::error(e)),
                                Err(e) => state_cb
                                    .lock()
                                    .unwrap()
                                    .push(LogMessage::error(format!("Internal error: {e}"))),
                            });
                    }
                    None => state
                        .lock()
                        .unwrap()
                        .push(LogMessage::error("Not spectating any game.")),
                }
            }

            "spectators" => {
                let gid = resolve_game_id(&parts, &state);
                match gid {
                    Some(id) => {
                        let specs: Vec<_> = conn
                            .db()
                            .spectator()
                            .iter()
                            .filter(|s| s.game_id == id)
                            .collect();
                        let mut st = state.lock().unwrap();
                        st.push(LogMessage::system(format!(
                            "── Spectators — Game #{} ──",
                            id
                        )));
                        if specs.is_empty() {
                            st.push(LogMessage::info("  (none)"));
                        }
                        for s in &specs {
                            st.push(LogMessage::info(format!("  {}", s.username)));
                        }
                    }
                    None => state
                        .lock()
                        .unwrap()
                        .push(LogMessage::error("Usage: spectators [game_id]")),
                }
            }

            // ── Chat ───────────────────────────────────────────────────────
            "chat" => {
                if parts.len() < 2 {
                    state
                        .lock()
                        .unwrap()
                        .push(LogMessage::error("Usage: chat <message>"));
                } else {
                    let text = parts[1..].join(" ");
                    let gid = state.lock().unwrap().active_game_id.unwrap_or(0);
                    let state_cb = Arc::clone(&state);
                    let _ = conn
                        .reducers()
                        .send_chat_then(gid, text, move |_ctx, result| {
                            reducer_callback(&state_cb, result);
                        });
                }
            }

            // ── Leaderboard ────────────────────────────────────────────────
            "leaderboard" => {
                let mut users: Vec<User> = conn.db().user().iter().collect();
                users.sort_by(|a, b| b.wins.cmp(&a.wins));
                let mut st = state.lock().unwrap();
                st.push(LogMessage::system("── Leaderboard ──"));
                st.push(LogMessage::info(format!(
                    "  {:<20} {:>5} {:>5} {:>5}",
                    "Player", "W", "L", "D"
                )));
                st.push(LogMessage::info(format!("  {}", "─".repeat(38))));
                for u in &users {
                    st.push(LogMessage::info(format!(
                        "  {:<20} {:>5} {:>5} {:>5}",
                        u.username, u.wins, u.losses, u.draws
                    )));
                }
            }

            other => {
                state.lock().unwrap().push(LogMessage::error(format!(
                    "Unknown command '{}'. Type 'help'.",
                    other
                )));
            }
        }
    }
}
// ─────────────────────────────────────────────
//  PASSWORD PROMPT  (via log + channel)
// ─────────────────────────────────────────────

static PASSWORD_TX: std::sync::LazyLock<Mutex<Option<mpsc::Sender<String>>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));
// static PASSWORD_RX: std::sync::LazyLock<Mutex<Option<mpsc::Receiver<String>>>> =
//     std::sync::LazyLock::new(|| Mutex::new(None));

fn prompt_password(state: &State, prompt: &str) -> String {
    let (tx, rx) = mpsc::channel();

    {
        let mut st = state.lock().unwrap();

        st.password_mode = true;
        st.password_buffer.clear();
        st.password_prompt = prompt.to_string();
    }

    *PASSWORD_TX.lock().unwrap() = Some(tx);

    rx.recv_timeout(Duration::from_secs(60)).unwrap_or_default()
}

// ─────────────────────────────────────────────
//  HELP TEXT
// ─────────────────────────────────────────────

const HELP_LINES: &[&str] = &[
    "── Commands ──────────────────────────────",
    " login <user>         Log in",
    " register <user>      Register",
    " logout               Log out",
    " whoami               Show account",
    " passwd               Change password",
    "──────────────────────────────────────────",
    " lobby                Show waiting players",
    " join                 Join matchmaking",
    " leave                Leave lobby",
    "──────────────────────────────────────────",
    " games                List active games",
    " game <id>            Set active game",
    " move <from> <to> [p] Make a move",
    " resign               Resign",
    " draw                 Offer/accept draw",
    "──────────────────────────────────────────",
    " history [id]         Move history",
    " pgn [id]             Export PGN",
    "──────────────────────────────────────────",
    " spectate <id>        Watch a game",
    " unspectate           Stop watching",
    " spectators [id]      List spectators",
    "──────────────────────────────────────────",
    " chat <msg>           Send chat",
    " leaderboard          Player rankings",
    " quit                 Exit",
    "  Tab: autocomplete   PgUp/PgDn: scroll",
];

// ─────────────────────────────────────────────
//  UTILITIES
// ─────────────────────────────────────────────

fn piece_unicode(ch: char) -> &'static str {
    match ch {
        'K' => "♔",
        'Q' => "♕",
        'R' => "♖",
        'B' => "♗",
        'N' => "♘",
        'P' => "♙",
        'k' => "♚",
        'q' => "♛",
        'r' => "♜",
        'b' => "♝",
        'n' => "♞",
        'p' => "♟",
        _ => "·",
    }
}

fn parse_square(s: &str) -> Result<(u8, u8), String> {
    let b = s.as_bytes();
    if b.len() != 2 {
        return Err(format!("'{}' is not a valid square (e.g. e4)", s));
    }
    let file = b[0].to_ascii_lowercase();
    let rank = b[1];
    if file < b'a' || file > b'h' {
        return Err("File out of range (a-h)".to_string());
    }
    if rank < b'1' || rank > b'8' {
        return Err("Rank out of range (1-8)".to_string());
    }
    Ok((file - b'a', rank - b'1'))
}

fn resolve_game_id(parts: &[&str], state: &State) -> Option<u64> {
    if parts.len() >= 2 {
        parts[1].parse::<u64>().ok()
    } else {
        let st = state.lock().unwrap();
        st.active_game_id.or(st.spectating_game_id)
    }
}
