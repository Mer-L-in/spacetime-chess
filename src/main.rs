mod module_bindings;
use module_bindings::*;

use spacetimedb_sdk::{DbContext, Identity, Table, TableWithPrimaryKey};
use std::env;
use std::io::{self, BufRead, Write};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

// ─────────────────────────────────────────────
//  SHARED CLIENT STATE
// ─────────────────────────────────────────────

#[derive(Default)]
struct ClientState {
    my_identity: Option<Identity>,
    my_user_id: Option<u64>,
    my_username: Option<String>,
    active_game_id: Option<u64>,
    spectating_game_id: Option<u64>,
    authenticated: bool,
    auth_signal: Option<mpsc::Sender<bool>>,
}

type State = Arc<Mutex<ClientState>>;

// ─────────────────────────────────────────────
//  MAIN
// ─────────────────────────────────────────────

fn main() {
    let host = env::var("SPACETIMEDB_HOST").unwrap_or_else(|_| "http://localhost:3000".to_string());
    let db_name = env::var("SPACETIMEDB_DB_NAME").unwrap_or_else(|_| "spacetime-chess".to_string());

    let state: State = Arc::new(Mutex::new(ClientState::default()));
    let conn_holder: Arc<Mutex<Option<Arc<DbConnection>>>> = Arc::new(Mutex::new(None));

    let conn_holder_clone = conn_holder.clone();
    let state_for_connect = state.clone();

    let conn = Arc::new(
        DbConnection::builder()
            .with_database_name(db_name)
            .with_uri(host)
            .on_connect(move |ctx, identity, _token| {
                println!("✓ Connected to SpacetimeDB\n");

                {
                    let mut st = state_for_connect.lock().unwrap();
                    st.my_identity = Some(identity);
                    st.authenticated = false;
                }

                ctx.subscription_builder()
                    .on_error(|_ctx, e| eprintln!("[subscription error] {e}"))
                    .add_query(|q| q.from.user())
                    .add_query(|q| q.from.game())
                    .add_query(|q| q.from.lobby_entry())
                    .add_query(|q| q.from.move_record())
                    .add_query(|q| q.from.spectator())
                    .add_query(|q| q.from.chat_message())
                    .add_query(|q| q.from.my_session())
                    .subscribe();

                // AUTH EVENTS
                ctx.db().my_session().on_insert({
                    let state = state_for_connect.clone();

                    move |_ctx, session| {
                        let mut st = state.lock().unwrap();

                        if Some(session.identity) == st.my_identity {
                            st.my_user_id = Some(session.user_id);
                            st.my_username = Some(session.username.clone());
                            st.authenticated = true;

                            println!(
                                "\n✓ Logged in as '{}'. Type 'help' for commands.\n",
                                session.username
                            );

                            if let Some(tx) = st.auth_signal.take() {
                                let _ = tx.send(true);
                            }
                        }
                    }
                });

                ctx.db().my_session().on_delete({
                    let state = state_for_connect.clone();

                    move |_ctx, session| {
                        let mut st = state.lock().unwrap();

                        if Some(session.identity) == st.my_identity {
                            st.my_user_id = None;
                            st.my_username = None;
                            st.authenticated = false;
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
                                // state.lock().unwrap().active_game_id = Some(game.id);
                                st.active_game_id = Some(game.id);
                                println!("\n🎮 Game #{} started! You are {}", game.id, my_color);
                                println!("  {} vs {}", game.white_username, game.black_username);

                                print_board_from_fen(&game, &Some(uid));
                                reprint_prompt(&st);
                                drop(st);
                            }
                        }
                    }
                });

                ctx.db().game().on_update({
                    let state = state_for_connect.clone();

                    move |_ctx, _old, g| {
                        let st = state.lock().unwrap();

                        let uid = st.my_user_id;
                        let watching =
                            st.active_game_id == Some(g.id) || st.spectating_game_id == Some(g.id);

                        let is_player =
                            uid.map_or(false, |u| u == g.white_user_id || u == g.black_user_id);

                        if watching || is_player {
                            print_board_from_fen(&g, &uid);

                            match g.status {
                                GameStatus::InProgress => {
                                    println!("  Turn: {:?}", g.turn);
                                }
                                GameStatus::Checkmate => println!("\n♛  Checkmate!"),
                                GameStatus::Stalemate => println!("\n½  Stalemate — draw"),
                                GameStatus::Draw => println!("\n½  Draw agreed"),
                                GameStatus::Resigned => println!("\n🏳  A player resigned"),
                                _ => {}
                            }
                            reprint_prompt(&st);
                        }
                    }
                });

                ctx.db().chat_message().on_insert({
                    let state = state_for_connect.clone();

                    move |_ctx, msg| {
                        let st = state.lock().unwrap();

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

                            println!("[{}] {}: {}", label, msg.sender_name, msg.text);
                            reprint_prompt(&st);
                        }
                    }
                });

                let conn_arc = conn_holder_clone.lock().unwrap().as_ref().unwrap().clone();
                let state = state_for_connect.clone();

                std::thread::spawn(move || {
                    auth_loop(&conn_arc, &state);
                    repl_loop(&conn_arc, &state);
                });
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

    loop {
        std::thread::park();
    }
}

// ─────────────────────────────────────────────
// AUTH LOOP
// ─────────────────────────────────────────────

fn auth_loop(conn: &DbConnection, state: &State) {
    println!("╔═══════════════════════════════════╗");
    println!("║ ♟ SpacetimeDB Chess ♟             ║");
    println!("╠═══════════════════════════════════╣");
    println!("║ login <username>                  ║");
    println!("║ register <username>               ║");
    println!("║ quit                              ║");
    println!("╚═══════════════════════════════════╝\n");
    let stdin = io::stdin();

    loop {
        if state.lock().unwrap().authenticated {
            return;
        }
        print!("auth> ");
        io::stdout().flush().unwrap();

        let mut line = String::new();
        stdin.lock().read_line(&mut line).unwrap();
        let parts: Vec<&str> = line.trim().split_whitespace().collect();

        match parts.get(0).copied() {
            Some("login") => {
                let username = parts.get(1).unwrap_or(&"").to_string();
                let password = read_password("Password");

                let (tx, rx) = mpsc::channel();
                state.lock().unwrap().auth_signal = Some(tx);

                match conn.reducers().login(username, password) {
                    Ok(_) => match rx.recv_timeout(Duration::from_secs(5)) {
                        Ok(true) => return,
                        _ => eprintln!("✗ Invalid login"),
                    },
                    Err(e) => eprintln!("Error: {e}"),
                }
            }

            Some("register") => {
                let username = parts.get(1).unwrap_or(&"").to_string();
                let password = read_password("Password");
                let confirm = read_password("Confirm");

                if password != confirm {
                    println!("Passwords do not match");
                    continue;
                }

                match conn.reducers().register(username.clone(), password.clone()) {
                    Ok(_) => {
                        println!("Registered. Logging in...");

                        let (tx, rx) = mpsc::channel();
                        state.lock().unwrap().auth_signal = Some(tx);

                        match conn.reducers().login(username, password) {
                            Ok(_) => match rx.recv_timeout(Duration::from_secs(5)) {
                                Ok(true) => return,
                                _ => eprintln!("Login failed"),
                            },
                            Err(e) => eprintln!("Error: {e}"),
                        }
                    }
                    Err(e) => eprintln!("Error: {e}"),
                }
            }

            Some("quit") => std::process::exit(0),
            _ => println!("Unknown command"),
        }
    }
}

// ─────────────────────────────────────────────
// REPL LOOP (FULL)
// ─────────────────────────────────────────────

fn repl_loop(conn: &DbConnection, state: &State) {
    let stdin = io::stdin();
    loop {
        if let Some(username) = state.lock().unwrap().my_username.as_ref() {
            print!("{}> ", username);
        }
        io::stdout().flush().unwrap();

        let mut line = String::new();
        stdin.lock().read_line(&mut line).unwrap();
        let parts: Vec<&str> = line.trim().split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }

        match parts[0] {
            "help" => print_help(),

            // ── Account ──────────────────────────────────────────────────
            "logout" => {
                match conn.reducers().logout() {
                    Ok(_) => {
                        {
                            let mut st = state.lock().unwrap();
                            st.my_user_id = None;
                            st.my_username = None;
                            st.authenticated = false;
                            st.active_game_id = None;
                            st.spectating_game_id = None;
                        }
                        println!("Logged out.");
                        // Return to auth loop
                        auth_loop(conn, &state);
                    }
                    Err(e) => eprintln!("Error: {e}"),
                }
            }

            "passwd" => {
                let current = read_password("  Current password");
                let new_pw = read_password("  New password (min 8 chars)");
                let confirm = read_password("  Confirm new password");
                if new_pw != confirm {
                    println!("  ✗ Passwords do not match.");
                } else {
                    match conn.reducers().change_password(current, new_pw) {
                        Ok(_) => println!("✓ Password changed."),
                        Err(e) => eprintln!("Error: {e}"),
                    }
                }
            }

            "whoami" => {
                let st = state.lock().unwrap();
                if let (Some(uid), Some(uname)) = (st.my_user_id, st.my_username.clone()) {
                    drop(st);
                    if let Some(user) = conn.db().user().iter().find(|u| u.user_id == uid) {
                        println!(
                            "  {} (user_id={})  W:{} L:{} D:{}",
                            user.username, user.user_id, user.wins, user.losses, user.draws
                        );
                    } else {
                        println!("  {} (user_id={})", uname, uid);
                    }
                }
            }

            // ── Lobby ─────────────────────────────────────────────────────
            "lobby" => print_lobby(conn),
            "join" => match conn.reducers().join_lobby() {
                Ok(_) => println!("Joined matchmaking lobby…"),
                Err(e) => eprintln!("Error: {e}"),
            },
            "leave" => match conn.reducers().leave_lobby() {
                Ok(_) => println!("Left lobby."),
                Err(e) => eprintln!("Error: {e}"),
            },

            // ── Games ─────────────────────────────────────────────────────
            "games" => print_games(conn),

            "game" => {
                if parts.len() < 2 {
                    println!("Usage: game <id>");
                } else if let Ok(id) = parts[1].parse::<u64>() {
                    state.lock().unwrap().active_game_id = Some(id);
                    let gid = Some(id);
                    println!("Active game set to #{}", id);
                    match gid {
                        Some(id) => {
                            if let Some(game) = conn.db().game().iter().find(|g| g.id == id) {
                                let uid = state.lock().unwrap().my_user_id;
                                print_board_from_fen(&game, &uid);
                            } else {
                                println!("Game {} not found.", id);
                            }
                        }
                        None => println!("No active game. Usage: game <id>]"),
                    }
                } else {
                    println!("Invalid game id.");
                }
            }

            "board" => {
                let gid = resolve_game_id(&parts, &state);
                match gid {
                    Some(id) => {
                        if let Some(game) = conn.db().game().iter().find(|g| g.id == id) {
                            let uid = state.lock().unwrap().my_user_id;
                            print_board_from_fen(&game, &uid);
                        } else {
                            println!("Game {} not found.", id);
                        }
                    }
                    None => println!("No active game. Usage: board [game_id]"),
                }
            }

            // ── Move ──────────────────────────────────────────────────────
            "move" => {
                if parts.len() < 3 {
                    println!("Usage: move <from> <to> [promotion]  e.g.  move e2 e4");
                    continue;
                }
                let gid = state.lock().unwrap().active_game_id;
                let Some(game_id) = gid else {
                    println!("No active game. Use 'games' then 'game <id>'.");
                    continue;
                };
                match (parse_square(parts[1]), parse_square(parts[2])) {
                    (Ok((ff, fr)), Ok((tf, tr))) => {
                        let promo = parts.get(3).map(|s| s.to_string());
                        match conn.reducers().make_move(game_id, ff, fr, tf, tr, promo) {
                            Ok(_) => {}
                            Err(e) => eprintln!("Illegal move: {e}"),
                        }
                    }
                    (Err(e), _) => eprintln!("Bad source square: {e}"),
                    (_, Err(e)) => eprintln!("Bad target square: {e}"),
                }
            }

            // ── Resign / Draw ─────────────────────────────────────────────
            "resign" => {
                let gid = state.lock().unwrap().active_game_id;
                match gid {
                    Some(id) => match conn.reducers().resign(id) {
                        Ok(_) => println!("You resigned."),
                        Err(e) => eprintln!("Error: {e}"),
                    },
                    None => println!("No active game."),
                }
            }

            "draw" => {
                let gid = state.lock().unwrap().active_game_id;
                match gid {
                    Some(id) => match conn.reducers().offer_draw(id) {
                        Ok(_) => println!("Draw offer sent (or accepted)."),
                        Err(e) => eprintln!("Error: {e}"),
                    },
                    None => println!("No active game."),
                }
            }

            // ── History / PGN ─────────────────────────────────────────────
            "history" => {
                let gid = resolve_game_id(&parts, &state);
                match gid {
                    Some(id) => print_history(conn, id),
                    None => println!("Usage: history [game_id]"),
                }
            }

            "pgn" => {
                let gid = resolve_game_id(&parts, &state);
                match gid {
                    Some(id) => print_pgn(conn, id),
                    None => println!("Usage: pgn [game_id]"),
                }
            }

            // ── Spectate ──────────────────────────────────────────────────
            "spectate" => {
                if parts.len() < 2 {
                    println!("Usage: spectate <game_id>");
                } else if let Ok(id) = parts[1].parse::<u64>() {
                    match conn.reducers().spectate_game(id) {
                        Ok(_) => {
                            state.lock().unwrap().spectating_game_id = Some(id);
                            println!("Now spectating game #{}", id);
                        }
                        Err(e) => eprintln!("Error: {e}"),
                    }
                } else {
                    println!("Invalid game id.");
                }
            }

            "unspectate" => {
                let gid = state.lock().unwrap().spectating_game_id;
                match gid {
                    Some(id) => match conn.reducers().leave_spectate(id) {
                        Ok(_) => {
                            state.lock().unwrap().spectating_game_id = None;
                            println!("Stopped spectating.");
                        }
                        Err(e) => eprintln!("Error: {e}"),
                    },
                    None => println!("Not spectating any game."),
                }
            }

            "spectators" => {
                let gid = resolve_game_id(&parts, &state);
                match gid {
                    Some(id) => print_spectators(conn, id),
                    None => println!("Usage: spectators [game_id]"),
                }
            }

            // ── Chat ──────────────────────────────────────────────────────
            "chat" => {
                if parts.len() < 2 {
                    println!("Usage: chat <message>");
                } else {
                    let text = parts[1..].join(" ");
                    let gid = state.lock().unwrap().active_game_id.unwrap_or(0);
                    match conn.reducers().send_chat(gid, text) {
                        Ok(_) => {}
                        Err(e) => eprintln!("Error: {e}"),
                    }
                }
            }

            // ── Leaderboard ───────────────────────────────────────────────
            "leaderboard" => print_leaderboard(conn),

            "quit" | "exit" => {
                println!("Goodbye!");
                std::process::exit(0);
            }

            other => println!("Unknown command '{}'. Type 'help'.", other),
        }
    }
}

// ─────────────────────────────────────────────
//  DISPLAY HELPERS
// ─────────────────────────────────────────────

fn print_help() {
    println!("\n╔════════════════════════════════════════════╗");
    println!("║           Chess CLI — Commands             ║");
    println!("╠════════════════════════════════════════════╣");
    println!("║ whoami               Show your account     ║");
    println!("║ passwd               Change password       ║");
    println!("║ logout               Log out               ║");
    println!("╠════════════════════════════════════════════╣");
    println!("║ lobby                Show waiting players  ║");
    println!("║ join                 Join matchmaking      ║");
    println!("║ leave-lobby          Leave lobby           ║");
    println!("╠════════════════════════════════════════════╣");
    println!("║ games                List active games     ║");
    println!("║ game <id>            Set active game       ║");
    println!("║ board [id]           Show board            ║");
    println!("║ move <from> <to> [p] Make a move           ║");
    println!("║   e.g.  move e2 e4                         ║");
    println!("║   e.g.  move e7 e8 Q  (promotion)          ║");
    println!("║ resign               Resign current game   ║");
    println!("║ draw                 Offer / accept draw   ║");
    println!("╠════════════════════════════════════════════╣");
    println!("║ history [id]         Move history          ║");
    println!("║ pgn [id]             Export PGN            ║");
    println!("╠════════════════════════════════════════════╣");
    println!("║ spectate <id>        Watch a game          ║");
    println!("║ unspectate           Stop watching         ║");
    println!("║ spectators [id]      List spectators       ║");
    println!("╠════════════════════════════════════════════╣");
    println!("║ chat <message>       Send chat             ║");
    println!("║ leaderboard          Player rankings       ║");
    println!("║ quit                 Exit                  ║");
    println!("╚════════════════════════════════════════════╝\n");
}

fn print_board_from_fen(game: &Game, uid: &Option<u64>) {
    let fen = &game.fen;
    let board_part = fen.split(' ').next().unwrap_or(&fen);
    let white_view = uid.map(|u| u == game.white_user_id).unwrap_or(true);

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
    println!();
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
    for rank in &ranks {
        print!(" {} ", rank + 1);
        for file in &files {
            let ch = squares[*rank as usize][*file as usize];
            let bg = if (rank + file) % 2 == 0 {
                "\x1b[48;5;94m"
            } else {
                "\x1b[48;5;136m"
            };
            print!("{} {} \x1b[0m", bg, piece_unicode(ch));
        }
        println!();
    }
    if white_view {
        println!("    a  b  c  d  e  f  g  h\n");
    } else {
        println!("    h  g  f  e  d  c  b  a\n");
    }
    let (turn_name, turn_color) = if game.turn == Color::White {
        (&game.white_username, "White")
    } else {
        (&game.black_username, "Black")
    };
    println!("  Turn: {} ({})", turn_name, turn_color);
    let my_turn = uid.is_some_and(|u| {
        (u == game.white_user_id && game.turn == Color::White)
            || (u == game.black_user_id && game.turn == Color::Black)
    });
    if my_turn {
        println!("  Use: move <from> <to> [promotion]\n");
    }
}

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

fn print_lobby(conn: &DbConnection) {
    println!("\n── Matchmaking Lobby ──");
    let entries: Vec<_> = conn
        .db()
        .lobby_entry()
        .iter()
        .filter(|e| e.status == LobbyStatus::Open)
        .collect();
    if entries.is_empty() {
        println!("  (lobby is empty)");
    }
    for e in &entries {
        println!("  [{}] {} — waiting", e.id, e.username);
    }
    println!();
}

fn print_games(conn: &DbConnection) {
    println!("\n── Active Games ──");
    let mut found = false;
    for g in conn.db().game().iter() {
        if g.status == GameStatus::InProgress || g.status == GameStatus::WaitingForOpponent {
            println!(
                "  #{} — {} (W) vs {} (B)  [turn: {:?}]",
                g.id, g.white_username, g.black_username, g.turn
            );
            found = true;
        }
    }
    if !found {
        println!("  (no active games)");
    }
    println!();
}

fn print_history(conn: &DbConnection, game_id: u64) {
    let mut moves: Vec<MoveRecord> = conn
        .db()
        .move_record()
        .iter()
        .filter(|m| m.game_id == game_id)
        .collect();
    moves.sort_by_key(|m| m.ply);
    if moves.is_empty() {
        println!("No moves for game #{}", game_id);
        return;
    }
    println!("\n── Move History — Game #{} ──", game_id);
    for m in &moves {
        let num = (m.ply + 1) / 2;
        let side = if m.piece_color == Color::White {
            "W"
        } else {
            "B"
        };
        let cap = if m.captured.is_some() { "x" } else { "-" };
        println!(
            "  {:>3}. [{}] {}{} {} {}{}  {}",
            num,
            side,
            (b'a' + m.from_file) as char,
            m.from_rank + 1,
            cap,
            (b'a' + m.to_file) as char,
            m.to_rank + 1,
            m.san
        );
    }
    println!();
}

fn print_pgn(conn: &DbConnection, game_id: u64) {
    let game = match conn.db().game().iter().find(|g| g.id == game_id) {
        Some(g) => g,
        None => {
            println!("Game #{} not found", game_id);
            return;
        }
    };
    let result_str = match game.status {
        GameStatus::Checkmate | GameStatus::Resigned => match game.winner_user_id {
            Some(w) if w == game.white_user_id => "1-0",
            Some(_) => "0-1",
            None => "*",
        },
        GameStatus::Draw | GameStatus::Stalemate => "1/2-1/2",
        _ => "*",
    };
    println!("\n── PGN — Game #{} ──", game_id);
    println!("[Event \"SpacetimeDB Chess\"]");
    println!("[Site \"spacetimedb\"]");
    println!("[White \"{}\"]", game.white_username);
    println!("[Black \"{}\"]", game.black_username);
    println!("[Result \"{}\"]", result_str);
    println!();
    let mut moves: Vec<MoveRecord> = conn
        .db()
        .move_record()
        .iter()
        .filter(|m| m.game_id == game_id)
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
    let mut col = 0;
    for token in pgn.split_whitespace() {
        if col + token.len() + 1 > 80 {
            println!();
            col = 0;
        }
        print!("{} ", token);
        col += token.len() + 1;
    }
    println!("\n");
}

fn print_spectators(conn: &DbConnection, game_id: u64) {
    let specs: Vec<_> = conn
        .db()
        .spectator()
        .iter()
        .filter(|s| s.game_id == game_id)
        .collect();
    if specs.is_empty() {
        println!("No spectators for game #{}", game_id);
        return;
    }
    println!("\n── Spectators — Game #{} ──", game_id);
    for s in &specs {
        println!("  {}", s.username);
    }
    println!();
}

fn print_leaderboard(conn: &DbConnection) {
    let mut users: Vec<User> = conn.db().user().iter().collect();
    users.sort_by(|a, b| b.wins.cmp(&a.wins));
    println!("\n── Leaderboard ──");
    println!("  {:<20} {:>5} {:>5} {:>5}", "Player", "W", "L", "D");
    println!("  {}", "─".repeat(38));
    for u in &users {
        println!(
            "  {:<20} {:>5} {:>5} {:>5}",
            u.username, u.wins, u.losses, u.draws
        );
    }
    println!();
}

// ─────────────────────────────────────────────
//  UTILITIES
// ─────────────────────────────────────────────

fn read_password(prompt: &str) -> String {
    print!("{}: ", prompt);
    io::stdout().flush().unwrap();
    rpassword::read_password().unwrap_or_default()
}

fn parse_square(s: &str) -> Result<(u8, u8), String> {
    let b = s.as_bytes();
    if b.len() != 2 {
        return Err(format!("'{}' is not a valid square (e.g. e4)", s));
    }
    let file = b[0].to_ascii_lowercase();
    let rank = b[1];
    if file < b'a' || file > b'h' {
        return Err(format!("File out of range (a-h)"));
    }
    if rank < b'1' || rank > b'8' {
        return Err(format!("Rank out of range (1-8)"));
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

fn reprint_prompt(state: &ClientState) {
    let prompt = match state.my_username.as_deref() {
        Some(name) if state.authenticated => format!("\n{}> ", name),
        _ => "\nauth> ".to_string(),
    };
    print!("{}", prompt);
    io::stdout().flush().unwrap();
}
