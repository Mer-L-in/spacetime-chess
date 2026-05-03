use spacetimedb::{rand::RngCore, ReducerContext, SpacetimeType, Table, Timestamp};

// ─────────────────────────────────────────────
//  CUSTOM TYPES
// ─────────────────────────────────────────────

#[derive(SpacetimeType, Clone, Debug, PartialEq, Copy)]
pub enum Color {
    White,
    Black,
}

#[derive(SpacetimeType, Clone, Debug, PartialEq, Copy)]
pub enum PieceKind {
    King,
    Queen,
    Rook,
    Bishop,
    Knight,
    Pawn,
}

#[derive(SpacetimeType, Clone, Debug, PartialEq)]
pub struct Piece {
    pub kind: PieceKind,
    pub color: Color,
}

#[derive(SpacetimeType, Clone, Debug, PartialEq, Copy)]
pub enum GameStatus {
    WaitingForOpponent,
    InProgress,
    Checkmate,
    Stalemate,
    Draw,
    Resigned,
}

#[derive(SpacetimeType, Clone, Debug, PartialEq, Copy)]
pub enum LobbyStatus {
    Open,
    Matched,
    Cancelled,
}

// ─────────────────────────────────────────────
//  AUTH TABLES
// ─────────────────────────────────────────────

/// Persistent user account — survives reconnects and is identity-independent.
#[spacetimedb::table(accessor = user, public)]
pub struct User {
    #[primary_key]
    #[auto_inc]
    pub user_id: u64,
    #[unique]
    pub username: String,
    pub password_hash: u64,
    pub wins: u32,
    pub losses: u32,
    pub draws: u32,
    pub created_at: Timestamp,
    pub last_login: Timestamp,
}

#[spacetimedb::table(accessor = my_session, public)]
pub struct MySession {
    #[primary_key]
    pub identity: spacetimedb::Identity,

    pub user_id: u64,
    pub username: String,
    pub logged_in_at: Timestamp,
}

/// Active session — maps a SpacetimeDB Identity (connection) to a User account.
/// Deleted on logout or client disconnect.
/// NOT public: only the server reads this.
#[spacetimedb::table(accessor = session)]
pub struct Session {
    #[primary_key]
    pub identity: spacetimedb::Identity,
    pub user_id: u64,
    pub logged_in_at: Timestamp,
}

// ─────────────────────────────────────────────
//  GAME TABLES
// ─────────────────────────────────────────────

/// Matchmaking lobby — open entries are waiting for an opponent.
#[spacetimedb::table(accessor = lobby_entry, public)]
pub struct LobbyEntry {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub user_id: u64,
    pub username: String,
    pub status: LobbyStatus,
    pub joined_at: Timestamp,
}

/// One row per game (active or finished).
#[spacetimedb::table(accessor = game, public)]
pub struct Game {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub white_user_id: u64,
    pub black_user_id: u64,
    pub white_username: String,
    pub black_username: String,
    /// Full FEN string — encodes complete board state.
    pub fen: String,
    pub status: GameStatus,
    pub turn: Color,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub winner_user_id: Option<u64>,
}

/// Every half-move ever played (append-only). Used for history and PGN export.
#[spacetimedb::table(
    accessor = move_record,
    public,
    index(name = "moves_by_game", accessor = moves_by_game, btree(columns = [game_id]))
)]
pub struct MoveRecord {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub game_id: u64,
    /// 1-based half-move count (ply).
    pub ply: u32,
    pub user_id: u64,
    pub from_file: u8,
    pub from_rank: u8,
    pub to_file: u8,
    pub to_rank: u8,
    /// Algebraic notation, e.g. "e4", "Nf3", "O-O".
    pub san: String,
    pub piece_kind: PieceKind,
    pub piece_color: Color,
    pub captured: Option<PieceKind>,
    pub promotion: Option<PieceKind>,
    pub is_check: bool,
    pub is_checkmate: bool,
    pub played_at: Timestamp,
}

/// Players (or anyone) spectating a game.
#[spacetimedb::table(
    accessor = spectator,
    public,
    index(name = "spectators_by_game", accessor = spectators_by_game, btree(columns = [game_id]))
)]
pub struct Spectator {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub game_id: u64,
    pub user_id: u64,
    pub username: String,
    pub joined_at: Timestamp,
}

/// In-game and lobby chat.
#[spacetimedb::table(
    accessor = chat_message,
    public,
    index(name = "chat_by_game", accessor = chat_by_game, btree(columns = [game_id]))
)]
pub struct ChatMessage {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    /// game_id = 0 means lobby chat.
    pub game_id: u64,
    pub user_id: u64,
    pub sender_name: String,
    pub text: String,
    pub sent_at: Timestamp,
}

/// Pending draw offers.
#[spacetimedb::table(accessor = draw_offer, public)]
pub struct DrawOffer {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub game_id: u64,
    pub offered_by_user_id: u64,
    pub offered_at: Timestamp,
}

// ─────────────────────────────────────────────
//  AUTH HELPERS
// ─────────────────────────────────────────────

/// FNV-64 hash — deterministic, no external deps, WASM-safe.
fn hash_password(password: &str) -> u64 {
    const FNV_OFFSET: u64 = 14695981039346656037;
    const FNV_PRIME: u64 = 1099511628211;
    let mut hash = FNV_OFFSET;
    for byte in password.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn validate_password(password: &str) -> Result<(), String> {
    if password.len() < 8 {
        return Err("Password must be at least 8 characters".to_string());
    }
    if password != password.trim() {
        return Err("Password must not have leading or trailing spaces".to_string());
    }
    Ok(())
}

/// Resolve the logged-in User for the current connection, or return an auth error.
/// Call this at the top of every reducer that requires authentication.
fn require_auth(ctx: &ReducerContext) -> Result<User, String> {
    let session = ctx.db.session().identity().find(ctx.sender()).ok_or(
        "Not logged in. Use 'login <username> <password>' or 'register <username> <password>'",
    )?;
    ctx.db
        .user()
        .user_id()
        .find(&session.user_id)
        .ok_or("Session refers to a deleted user — please log in again".to_string())
}

// ─────────────────────────────────────────────
//  CHESS ENGINE
// ─────────────────────────────────────────────

const STARTING_FEN: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";

struct Board {
    squares: [[Option<Piece>; 8]; 8],
    active: Color,
    castling: String,
    ep_file: Option<u8>,
    ep_rank: Option<u8>,
    halfmove: u32,
    fullmove: u32,
}

impl Board {
    fn from_fen(fen: &str) -> Result<Board, String> {
        let p: Vec<&str> = fen.split(' ').collect();
        if p.len() < 6 {
            return Err(format!("Invalid FEN (need 6 parts): {}", fen));
        }
        let mut squares: [[Option<Piece>; 8]; 8] = Default::default();
        for (ri, row) in p[0].split('/').enumerate() {
            let rank = 7 - ri;
            let mut file = 0usize;
            for ch in row.chars() {
                if let Some(n) = ch.to_digit(10) {
                    file += n as usize;
                } else {
                    let color = if ch.is_uppercase() {
                        Color::White
                    } else {
                        Color::Black
                    };
                    let kind = match ch.to_ascii_lowercase() {
                        'k' => PieceKind::King,
                        'q' => PieceKind::Queen,
                        'r' => PieceKind::Rook,
                        'b' => PieceKind::Bishop,
                        'n' => PieceKind::Knight,
                        'p' => PieceKind::Pawn,
                        c => return Err(format!("Unknown piece char '{}'", c)),
                    };
                    squares[rank][file] = Some(Piece { kind, color });
                    file += 1;
                }
            }
        }
        let active = match p[1] {
            "w" => Color::White,
            "b" => Color::Black,
            _ => return Err("Invalid active color".to_string()),
        };
        let (ep_file, ep_rank) = if p[3] == "-" {
            (None, None)
        } else {
            let bytes = p[3].as_bytes();
            if bytes.len() == 2 {
                (
                    Some(bytes[0].wrapping_sub(b'a')),
                    Some(bytes[1].wrapping_sub(b'1')),
                )
            } else {
                (None, None)
            }
        };
        Ok(Board {
            squares,
            active,
            castling: p[2].to_string(),
            ep_file,
            ep_rank,
            halfmove: p[4].parse().unwrap_or(0),
            fullmove: p[5].parse().unwrap_or(1),
        })
    }

    fn get(&self, file: u8, rank: u8) -> Option<&Piece> {
        if file < 8 && rank < 8 {
            self.squares[rank as usize][file as usize].as_ref()
        } else {
            None
        }
    }

    fn to_fen(&self) -> String {
        let mut rows = Vec::new();
        for rank in (0..8u8).rev() {
            let mut row = String::new();
            let mut empty = 0u8;
            for file in 0..8u8 {
                match &self.squares[rank as usize][file as usize] {
                    None => empty += 1,
                    Some(p) => {
                        if empty > 0 {
                            row.push((b'0' + empty) as char);
                            empty = 0;
                        }
                        let c = match p.kind {
                            PieceKind::King => 'k',
                            PieceKind::Queen => 'q',
                            PieceKind::Rook => 'r',
                            PieceKind::Bishop => 'b',
                            PieceKind::Knight => 'n',
                            PieceKind::Pawn => 'p',
                        };
                        row.push(if p.color == Color::White {
                            c.to_ascii_uppercase()
                        } else {
                            c
                        });
                    }
                }
            }
            if empty > 0 {
                row.push((b'0' + empty) as char);
            }
            rows.push(row);
        }
        let ep = match (self.ep_file, self.ep_rank) {
            (Some(f), Some(r)) => format!("{}{}", (b'a' + f) as char, (b'1' + r) as char),
            _ => "-".to_string(),
        };
        format!(
            "{} {} {} {} {} {}",
            rows.join("/"),
            match self.active {
                Color::White => "w",
                Color::Black => "b",
            },
            if self.castling.is_empty() {
                "-"
            } else {
                &self.castling
            },
            ep,
            self.halfmove,
            self.fullmove,
        )
    }

    fn apply_move(
        &mut self,
        ff: u8,
        fr: u8,
        tf: u8,
        tr: u8,
        promotion: Option<PieceKind>,
    ) -> Result<(Option<PieceKind>, bool, bool), String> {
        let piece = self.squares[fr as usize][ff as usize]
            .clone()
            .ok_or("No piece at source square")?;
        if piece.color != self.active {
            return Err("It is not your turn".to_string());
        }
        if tf >= 8 || tr >= 8 {
            return Err("Target square out of bounds".to_string());
        }
        if let Some(t) = self.get(tf, tr) {
            if t.color == piece.color {
                return Err("Cannot capture your own piece".to_string());
            }
        }

        let captured_kind = self.squares[tr as usize][tf as usize]
            .as_ref()
            .map(|p| p.kind);
        let mut is_ep = false;
        let mut is_castle = false;

        match piece.kind {
            PieceKind::Pawn => {
                let dir: i8 = if piece.color == Color::White { 1 } else { -1 };
                let df = tf as i8 - ff as i8;
                let dr = tr as i8 - fr as i8;
                if df == 0 && dr == dir {
                    if self.get(tf, tr).is_some() {
                        return Err("Pawn blocked".to_string());
                    }
                } else if df == 0 && dr == 2 * dir {
                    let start = if piece.color == Color::White {
                        1u8
                    } else {
                        6u8
                    };
                    if fr != start {
                        return Err("Pawn can only double-advance from starting rank".to_string());
                    }
                    let mid = (fr as i8 + dir) as u8;
                    if self.get(ff, mid).is_some() || self.get(tf, tr).is_some() {
                        return Err("Pawn path blocked".to_string());
                    }
                } else if df.abs() == 1 && dr == dir {
                    if self.get(tf, tr).is_none() {
                        if self.ep_file == Some(tf) && self.ep_rank == Some(tr) {
                            is_ep = true;
                        } else {
                            return Err("Pawn cannot move diagonally without capturing".to_string());
                        }
                    }
                } else {
                    return Err(format!(
                        "Invalid pawn move ({},{}) -> ({},{})",
                        ff, fr, tf, tr
                    ));
                }
                let back = if piece.color == Color::White {
                    7u8
                } else {
                    0u8
                };
                if tr == back && promotion.is_none() {
                    return Err("Must specify promotion piece".to_string());
                }
            }
            PieceKind::Knight => {
                let df = (tf as i8 - ff as i8).abs();
                let dr = (tr as i8 - fr as i8).abs();
                if !((df == 1 && dr == 2) || (df == 2 && dr == 1)) {
                    return Err("Invalid knight move".to_string());
                }
            }
            PieceKind::Bishop => {
                let df = (tf as i8 - ff as i8).abs();
                let dr = (tr as i8 - fr as i8).abs();
                if df != dr || df == 0 {
                    return Err("Invalid bishop move".to_string());
                }
                self.check_ray_clear(ff, fr, tf, tr)?;
            }
            PieceKind::Rook => {
                if ff != tf && fr != tr {
                    return Err("Invalid rook move".to_string());
                }
                self.check_ray_clear(ff, fr, tf, tr)?;
            }
            PieceKind::Queen => {
                let df = (tf as i8 - ff as i8).abs();
                let dr = (tr as i8 - fr as i8).abs();
                if ff != tf && fr != tr && df != dr {
                    return Err("Invalid queen move".to_string());
                }
                self.check_ray_clear(ff, fr, tf, tr)?;
            }
            PieceKind::King => {
                let df = (tf as i8 - ff as i8).abs();
                let dr = (tr as i8 - fr as i8).abs();
                if df == 2 && dr == 0 {
                    is_castle = true;
                    self.validate_castle(ff, fr, tf, &piece)?;
                } else if df > 1 || dr > 1 {
                    return Err("Invalid king move".to_string());
                }
            }
        }

        if is_ep {
            let cap_rank = (tr as i8
                - if piece.color == Color::White {
                    1i8
                } else {
                    -1i8
                }) as u8;
            self.squares[cap_rank as usize][tf as usize] = None;
        }
        let moved_piece = if let Some(k) = promotion {
            Piece {
                kind: k,
                color: piece.color,
            }
        } else {
            piece.clone()
        };
        self.squares[tr as usize][tf as usize] = Some(moved_piece);
        self.squares[fr as usize][ff as usize] = None;

        if is_castle {
            let (rf, rt) = if tf > ff { (7u8, 5u8) } else { (0u8, 3u8) };
            let rook = self.squares[fr as usize][rf as usize].take();
            self.squares[tr as usize][rt as usize] = rook;
        }

        if piece.kind == PieceKind::Pawn && (tr as i8 - fr as i8).abs() == 2 {
            let ep_r = (fr as i8
                + if piece.color == Color::White {
                    1i8
                } else {
                    -1i8
                }) as u8;
            self.ep_file = Some(ff);
            self.ep_rank = Some(ep_r);
        } else {
            self.ep_file = None;
            self.ep_rank = None;
        }

        self.update_castling_rights(ff, fr, tf, tr, &piece);
        if piece.kind == PieceKind::Pawn || captured_kind.is_some() {
            self.halfmove = 0;
        } else {
            self.halfmove += 1;
        }
        if self.active == Color::Black {
            self.fullmove += 1;
        }
        self.active = match self.active {
            Color::White => Color::Black,
            Color::Black => Color::White,
        };
        Ok((captured_kind, is_ep, is_castle))
    }

    fn check_ray_clear(&self, ff: u8, fr: u8, tf: u8, tr: u8) -> Result<(), String> {
        let df = (tf as i8 - ff as i8).signum();
        let dr = (tr as i8 - fr as i8).signum();
        let mut f = ff as i8 + df;
        let mut r = fr as i8 + dr;
        while (f, r) != (tf as i8, tr as i8) {
            if self.get(f as u8, r as u8).is_some() {
                return Err("Path is blocked".to_string());
            }
            f += df;
            r += dr;
        }
        Ok(())
    }

    fn validate_castle(&self, ff: u8, fr: u8, tf: u8, piece: &Piece) -> Result<(), String> {
        let kingside = tf > ff;
        let right = match (piece.color, kingside) {
            (Color::White, true) => 'K',
            (Color::White, false) => 'Q',
            (Color::Black, true) => 'k',
            (Color::Black, false) => 'q',
        };
        if !self.castling.contains(right) {
            return Err("Castling not available".to_string());
        }
        let (start, end) = if kingside {
            (ff + 1, 7u8)
        } else {
            (1u8, ff - 1)
        };
        for f in start..end {
            if self.get(f, fr).is_some() {
                return Err("Castling path blocked".to_string());
            }
        }
        Ok(())
    }

    fn update_castling_rights(&mut self, ff: u8, fr: u8, tf: u8, tr: u8, piece: &Piece) {
        match (piece.kind, piece.color) {
            (PieceKind::King, Color::White) => {
                self.castling.retain(|c| c != 'K' && c != 'Q');
            }
            (PieceKind::King, Color::Black) => {
                self.castling.retain(|c| c != 'k' && c != 'q');
            }
            (PieceKind::Rook, Color::White) => {
                if ff == 7 && fr == 0 {
                    self.castling.retain(|c| c != 'K');
                }
                if ff == 0 && fr == 0 {
                    self.castling.retain(|c| c != 'Q');
                }
            }
            (PieceKind::Rook, Color::Black) => {
                if ff == 7 && fr == 7 {
                    self.castling.retain(|c| c != 'k');
                }
                if ff == 0 && fr == 7 {
                    self.castling.retain(|c| c != 'q');
                }
            }
            _ => {}
        }
        if tf == 7 && tr == 0 {
            self.castling.retain(|c| c != 'K');
        }
        if tf == 0 && tr == 0 {
            self.castling.retain(|c| c != 'Q');
        }
        if tf == 7 && tr == 7 {
            self.castling.retain(|c| c != 'k');
        }
        if tf == 0 && tr == 7 {
            self.castling.retain(|c| c != 'q');
        }
    }

    fn king_in_check(&self, color: Color) -> bool {
        let mut kf = 0u8;
        let mut kr = 0u8;
        'outer: for rank in 0..8u8 {
            for file in 0..8u8 {
                if let Some(p) = self.get(file, rank) {
                    if p.kind == PieceKind::King && p.color == color {
                        kf = file;
                        kr = rank;
                        break 'outer;
                    }
                }
            }
        }
        let opp = match color {
            Color::White => Color::Black,
            Color::Black => Color::White,
        };
        for (df, dr) in &[
            (1i8, 2i8),
            (2, 1),
            (2, -1),
            (1, -2),
            (-1, -2),
            (-2, -1),
            (-2, 1),
            (-1, 2),
        ] {
            let f = kf as i8 + df;
            let r = kr as i8 + dr;
            if f >= 0 && f < 8 && r >= 0 && r < 8 {
                if let Some(p) = self.get(f as u8, r as u8) {
                    if p.color == opp && p.kind == PieceKind::Knight {
                        return true;
                    }
                }
            }
        }
        for (df, dr) in &[(1i8, 0i8), (-1, 0), (0, 1), (0, -1i8)] {
            let mut f = kf as i8 + df;
            let mut r = kr as i8 + dr;
            while f >= 0 && f < 8 && r >= 0 && r < 8 {
                if let Some(p) = self.get(f as u8, r as u8) {
                    if p.color == opp && (p.kind == PieceKind::Rook || p.kind == PieceKind::Queen) {
                        return true;
                    }
                    break;
                }
                f += df;
                r += dr;
            }
        }
        for (df, dr) in &[(1i8, 1i8), (1, -1), (-1, 1), (-1, -1i8)] {
            let mut f = kf as i8 + df;
            let mut r = kr as i8 + dr;
            while f >= 0 && f < 8 && r >= 0 && r < 8 {
                if let Some(p) = self.get(f as u8, r as u8) {
                    if p.color == opp && (p.kind == PieceKind::Bishop || p.kind == PieceKind::Queen)
                    {
                        return true;
                    }
                    break;
                }
                f += df;
                r += dr;
            }
        }
        let pd: i8 = if color == Color::White { 1 } else { -1 };
        for df in &[-1i8, 1] {
            let f = kf as i8 + df;
            let r = kr as i8 + pd;
            if f >= 0 && f < 8 && r >= 0 && r < 8 {
                if let Some(p) = self.get(f as u8, r as u8) {
                    if p.color == opp && p.kind == PieceKind::Pawn {
                        return true;
                    }
                }
            }
        }
        for df in -1i8..=1 {
            for dr in -1i8..=1 {
                if df == 0 && dr == 0 {
                    continue;
                }
                let f = kf as i8 + df;
                let r = kr as i8 + dr;
                if f >= 0 && f < 8 && r >= 0 && r < 8 {
                    if let Some(p) = self.get(f as u8, r as u8) {
                        if p.color == opp && p.kind == PieceKind::King {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    fn build_san(
        &self,
        ff: u8,
        _fr: u8,
        tf: u8,
        tr: u8,
        piece: &Piece,
        captured: Option<PieceKind>,
        is_ep: bool,
        is_castle: bool,
        promotion: Option<PieceKind>,
        is_check: bool,
        is_checkmate: bool,
    ) -> String {
        if is_castle {
            let s = if tf > ff { "O-O" } else { "O-O-O" };
            return format!(
                "{}{}",
                s,
                if is_checkmate {
                    "#"
                } else if is_check {
                    "+"
                } else {
                    ""
                }
            );
        }
        let mut san = String::new();
        san.push_str(match piece.kind {
            PieceKind::King => "K",
            PieceKind::Queen => "Q",
            PieceKind::Rook => "R",
            PieceKind::Bishop => "B",
            PieceKind::Knight => "N",
            PieceKind::Pawn => "",
        });
        if piece.kind == PieceKind::Pawn && (captured.is_some() || is_ep) {
            san.push((b'a' + ff) as char);
        }
        if captured.is_some() || is_ep {
            san.push('x');
        }
        san.push((b'a' + tf) as char);
        san.push((b'1' + tr) as char);
        if let Some(promo) = promotion {
            san.push('=');
            san.push_str(match promo {
                PieceKind::Queen => "Q",
                PieceKind::Rook => "R",
                PieceKind::Bishop => "B",
                PieceKind::Knight => "N",
                _ => "Q",
            });
        }
        if is_checkmate {
            san.push('#');
        } else if is_check {
            san.push('+');
        }
        san
    }
}

// ─────────────────────────────────────────────
//  LIFECYCLE REDUCERS
// ─────────────────────────────────────────────

#[spacetimedb::reducer(init)]
pub fn init(_ctx: &ReducerContext) {
    log::info!("Chess module initialised");
}

#[spacetimedb::reducer(client_connected)]
pub fn identity_connected(_ctx: &ReducerContext) {
    // Session is created explicitly via login/register — nothing automatic here.
}

/// Clean up the session on disconnect so the identity slot is freed.
#[spacetimedb::reducer(client_disconnected)]
pub fn identity_disconnected(ctx: &ReducerContext) {
    if let Some(session) = ctx.db.session().identity().find(ctx.sender()) {
        ctx.db.session().identity().delete(&ctx.sender());
        log::info!("Session ended for user_id={}", session.user_id);
    }
}

// ─────────────────────────────────────────────
//  AUTH REDUCERS
// ─────────────────────────────────────────────

/// Create a new account and immediately log in.
#[spacetimedb::reducer]
pub fn register(ctx: &ReducerContext, username: String, password: String) -> Result<(), String> {
    let username = username.trim().to_string();
    if username.is_empty() {
        return Err("Username cannot be empty".to_string());
    }
    if username.len() > 24 {
        return Err("Username too long (max 24 chars)".to_string());
    }
    if username.contains(|c: char| !c.is_alphanumeric() && c != '_' && c != '-') {
        return Err("Username may only contain letters, numbers, _ and -".to_string());
    }
    validate_password(&password)?;

    if ctx.db.user().username().find(&username).is_some() {
        return Err(format!("Username '{}' is already taken", username));
    }

    let user = ctx.db.user().insert(User {
        user_id: 0,
        username: username.clone(),
        password_hash: hash_password(&password),
        wins: 0,
        losses: 0,
        draws: 0,
        created_at: ctx.timestamp,
        last_login: ctx.timestamp,
    });

    // Auto-login: replace any stale session for this connection
    if ctx.db.session().identity().find(ctx.sender()).is_some() {
        ctx.db.session().identity().delete(&ctx.sender());
    }
    ctx.db.session().insert(Session {
        identity: ctx.sender(),
        user_id: user.user_id,
        logged_in_at: ctx.timestamp,
    });

    log::info!("Registered '{}' (user_id={})", username, user.user_id);
    Ok(())
}

/// Log in to an existing account.
#[spacetimedb::reducer]
pub fn login(ctx: &ReducerContext, username: String, password: String) -> Result<(), String> {
    let username = username.trim().to_string();

    let user = ctx
        .db
        .user()
        .username()
        .find(&username)
        .ok_or("Invalid username")?;

    if user.password_hash != hash_password(&password) {
        return Err("Invalid username or password".to_string());
    }

    // --- PRIVATE SESSION ---
    if ctx.db.session().identity().find(ctx.sender()).is_some() {
        ctx.db.session().identity().delete(&ctx.sender());
    }

    ctx.db.session().insert(Session {
        identity: ctx.sender(),
        user_id: user.user_id,
        logged_in_at: ctx.timestamp,
    });

    // --- PUBLIC VIEW (IMPORTANT PART) ---
    if ctx.db.my_session().identity().find(ctx.sender()).is_some() {
        ctx.db.my_session().identity().delete(&ctx.sender());
    }

    ctx.db.my_session().insert(MySession {
        identity: ctx.sender(),
        user_id: user.user_id,
        username: user.username.clone(),
        logged_in_at: ctx.timestamp,
    });

    // update last login
    ctx.db.user().user_id().update(User {
        last_login: ctx.timestamp,
        ..user
    });

    log::info!("User '{}' logged in (user_id={})", username, user.user_id);

    Ok(())
}

/// Log out — destroys the current session.
#[spacetimedb::reducer]
pub fn logout(ctx: &ReducerContext) -> Result<(), String> {
    ctx.db
        .session()
        .identity()
        .find(ctx.sender())
        .ok_or("Not logged in")?;

    ctx.db.session().identity().delete(&ctx.sender());
    ctx.db.my_session().identity().delete(&ctx.sender());
    Ok(())
}

/// Change password. Requires the current password for verification.
#[spacetimedb::reducer]
pub fn change_password(
    ctx: &ReducerContext,
    current_password: String,
    new_password: String,
) -> Result<(), String> {
    let user = require_auth(ctx)?;
    if user.password_hash != hash_password(&current_password) {
        return Err("Current password is incorrect".to_string());
    }
    validate_password(&new_password)?;
    if current_password == new_password {
        return Err("New password must differ from current password".to_string());
    }
    ctx.db.user().user_id().update(User {
        password_hash: hash_password(&new_password),
        ..user
    });
    Ok(())
}

// ─────────────────────────────────────────────
//  MATCHMAKING REDUCERS
// ─────────────────────────────────────────────

#[spacetimedb::reducer]
pub fn join_lobby(ctx: &ReducerContext) -> Result<(), String> {
    let user = require_auth(ctx)?;

    for entry in ctx.db.lobby_entry().iter() {
        if entry.user_id == user.user_id && entry.status == LobbyStatus::Open {
            return Err("Already in lobby".to_string());
        }
    }

    let opponent = ctx
        .db
        .lobby_entry()
        .iter()
        .find(|e| e.status == LobbyStatus::Open && e.user_id != user.user_id);

    if let Some(opp) = opponent {
        let (white_user_id, white_username, black_user_id, black_username) =
            if ctx.rng().next_u64() % 2 == 0 {
                (
                    user.user_id,
                    user.username.clone(),
                    opp.user_id,
                    opp.username.clone(),
                )
            } else {
                (
                    opp.user_id,
                    opp.username.clone(),
                    user.user_id,
                    user.username.clone(),
                )
            };

        ctx.db.lobby_entry().id().update(LobbyEntry {
            status: LobbyStatus::Matched,
            ..opp
        });
        ctx.db.lobby_entry().insert(LobbyEntry {
            id: 0,
            user_id: user.user_id,
            username: user.username.clone(),
            status: LobbyStatus::Matched,
            joined_at: ctx.timestamp,
        });
        ctx.db.game().insert(Game {
            id: 0,
            white_user_id,
            black_user_id,
            white_username,
            black_username,
            fen: STARTING_FEN.to_string(),
            status: GameStatus::InProgress,
            turn: Color::White,
            created_at: ctx.timestamp,
            updated_at: ctx.timestamp,
            winner_user_id: None,
        });
    } else {
        ctx.db.lobby_entry().insert(LobbyEntry {
            id: 0,
            user_id: user.user_id,
            username: user.username.clone(),
            status: LobbyStatus::Open,
            joined_at: ctx.timestamp,
        });
        log::info!("'{}' is waiting in lobby", user.username);
    }
    Ok(())
}

#[spacetimedb::reducer]
pub fn leave_lobby(ctx: &ReducerContext) -> Result<(), String> {
    let user = require_auth(ctx)?;
    let entry = ctx
        .db
        .lobby_entry()
        .iter()
        .find(|e| e.user_id == user.user_id && e.status == LobbyStatus::Open)
        .ok_or("Not in lobby")?;
    ctx.db.lobby_entry().id().update(LobbyEntry {
        status: LobbyStatus::Cancelled,
        ..entry
    });
    Ok(())
}

// ─────────────────────────────────────────────
//  GAME REDUCERS
// ─────────────────────────────────────────────

#[spacetimedb::reducer]
pub fn make_move(
    ctx: &ReducerContext,
    game_id: u64,
    from_file: u8,
    from_rank: u8,
    to_file: u8,
    to_rank: u8,
    promotion: Option<String>,
) -> Result<(), String> {
    let user = require_auth(ctx)?;
    let game = ctx.db.game().id().find(&game_id).ok_or("Game not found")?;

    if game.status != GameStatus::InProgress {
        return Err("Game is not in progress".to_string());
    }
    let player_color = if user.user_id == game.white_user_id {
        Color::White
    } else if user.user_id == game.black_user_id {
        Color::Black
    } else {
        return Err("You are not a player in this game".to_string());
    };
    if player_color != game.turn {
        return Err("It is not your turn".to_string());
    }

    let promo_kind: Option<PieceKind> = match promotion.as_deref() {
        Some("q") | Some("Q") => Some(PieceKind::Queen),
        Some("r") | Some("R") => Some(PieceKind::Rook),
        Some("b") | Some("B") => Some(PieceKind::Bishop),
        Some("n") | Some("N") => Some(PieceKind::Knight),
        None => None,
        Some(other) => return Err(format!("Unknown promotion piece '{}'", other)),
    };

    let mut board = Board::from_fen(&game.fen)?;
    let moving_piece = board
        .get(from_file, from_rank)
        .cloned()
        .ok_or("No piece at source square")?;
    let (captured, is_ep, is_castle) =
        board.apply_move(from_file, from_rank, to_file, to_rank, promo_kind)?;

    if board.king_in_check(player_color) {
        return Err("Move leaves king in check".to_string());
    }

    let opp = match player_color {
        Color::White => Color::Black,
        Color::Black => Color::White,
    };
    let is_check = board.king_in_check(opp);
    let is_checkmate = false; // Full legal-move gen needed — extension point

    let ply = ctx
        .db
        .move_record()
        .moves_by_game()
        .filter(&game_id)
        .count() as u32
        + 1;
    let san = Board::from_fen(&game.fen).unwrap().build_san(
        from_file,
        from_rank,
        to_file,
        to_rank,
        &moving_piece,
        captured,
        is_ep,
        is_castle,
        promo_kind,
        is_check,
        is_checkmate,
    );

    ctx.db.move_record().insert(MoveRecord {
        id: 0,
        game_id,
        ply,
        user_id: user.user_id,
        from_file,
        from_rank,
        to_file,
        to_rank,
        san,
        piece_kind: moving_piece.kind,
        piece_color: moving_piece.color,
        captured,
        promotion: promo_kind,
        is_check,
        is_checkmate,
        played_at: ctx.timestamp,
    });

    let new_turn = match game.turn {
        Color::White => Color::Black,
        Color::Black => Color::White,
    };
    ctx.db.game().id().update(Game {
        fen: board.to_fen(),
        status: if is_checkmate {
            GameStatus::Checkmate
        } else {
            GameStatus::InProgress
        },
        turn: new_turn,
        updated_at: ctx.timestamp,
        winner_user_id: if is_checkmate {
            Some(user.user_id)
        } else {
            None
        },
        ..game
    });

    if is_checkmate {
        record_result(ctx, game_id, Some(user.user_id));
    }
    Ok(())
}

#[spacetimedb::reducer]
pub fn offer_draw(ctx: &ReducerContext, game_id: u64) -> Result<(), String> {
    let user = require_auth(ctx)?;
    let game = ctx.db.game().id().find(&game_id).ok_or("Game not found")?;
    if game.status != GameStatus::InProgress {
        return Err("Game is not in progress".to_string());
    }
    if user.user_id != game.white_user_id && user.user_id != game.black_user_id {
        return Err("Not a player in this game".to_string());
    }

    let opponent_offered = ctx
        .db
        .draw_offer()
        .iter()
        .any(|o| o.game_id == game_id && o.offered_by_user_id != user.user_id);

    if opponent_offered {
        ctx.db.game().id().update(Game {
            status: GameStatus::Draw,
            updated_at: ctx.timestamp,
            ..game
        });
        record_result(ctx, game_id, None);
        for offer in ctx
            .db
            .draw_offer()
            .iter()
            .filter(|o| o.game_id == game_id)
            .collect::<Vec<_>>()
        {
            ctx.db.draw_offer().id().delete(&offer.id);
        }
    } else {
        ctx.db.draw_offer().insert(DrawOffer {
            id: 0,
            game_id,
            offered_by_user_id: user.user_id,
            offered_at: ctx.timestamp,
        });
    }
    Ok(())
}

#[spacetimedb::reducer]
pub fn resign(ctx: &ReducerContext, game_id: u64) -> Result<(), String> {
    let user = require_auth(ctx)?;
    let game = ctx.db.game().id().find(&game_id).ok_or("Game not found")?;
    if game.status != GameStatus::InProgress {
        return Err("Game is not in progress".to_string());
    }
    let winner = if user.user_id == game.white_user_id {
        game.black_user_id
    } else if user.user_id == game.black_user_id {
        game.white_user_id
    } else {
        return Err("Not a player in this game".to_string());
    };
    ctx.db.game().id().update(Game {
        status: GameStatus::Resigned,
        winner_user_id: Some(winner),
        updated_at: ctx.timestamp,
        ..game
    });
    record_result(ctx, game_id, Some(winner));
    Ok(())
}

// ─────────────────────────────────────────────
//  SPECTATOR REDUCERS
// ─────────────────────────────────────────────

#[spacetimedb::reducer]
pub fn spectate_game(ctx: &ReducerContext, game_id: u64) -> Result<(), String> {
    let user = require_auth(ctx)?;
    ctx.db.game().id().find(&game_id).ok_or("Game not found")?;
    if ctx
        .db
        .spectator()
        .spectators_by_game()
        .filter(&game_id)
        .any(|s| s.user_id == user.user_id)
    {
        return Err("Already spectating".to_string());
    }
    ctx.db.spectator().insert(Spectator {
        id: 0,
        game_id,
        user_id: user.user_id,
        username: user.username,
        joined_at: ctx.timestamp,
    });
    Ok(())
}

#[spacetimedb::reducer]
pub fn leave_spectate(ctx: &ReducerContext, game_id: u64) -> Result<(), String> {
    let user = require_auth(ctx)?;
    let entry = ctx
        .db
        .spectator()
        .spectators_by_game()
        .filter(&game_id)
        .find(|s| s.user_id == user.user_id)
        .ok_or("Not spectating this game")?;
    ctx.db.spectator().id().delete(&entry.id);
    Ok(())
}

// ─────────────────────────────────────────────
//  CHAT REDUCER
// ─────────────────────────────────────────────

#[spacetimedb::reducer]
pub fn send_chat(ctx: &ReducerContext, game_id: u64, text: String) -> Result<(), String> {
    let user = require_auth(ctx)?;
    if text.trim().is_empty() {
        return Err("Message cannot be empty".to_string());
    }
    if text.len() > 500 {
        return Err("Message too long".to_string());
    }
    ctx.db.chat_message().insert(ChatMessage {
        id: 0,
        game_id,
        user_id: user.user_id,
        sender_name: user.username,
        text,
        sent_at: ctx.timestamp,
    });
    Ok(())
}

// ─────────────────────────────────────────────
//  HELPERS
// ─────────────────────────────────────────────

fn record_result(ctx: &ReducerContext, game_id: u64, winner_user_id: Option<u64>) {
    let game = match ctx.db.game().id().find(&game_id) {
        Some(g) => g,
        None => return,
    };
    let update = |uid: u64, win: bool, draw: bool| {
        if let Some(u) = ctx.db.user().user_id().find(&uid) {
            ctx.db.user().user_id().update(User {
                wins: u.wins + if win { 1 } else { 0 },
                losses: u.losses + if !win && !draw { 1 } else { 0 },
                draws: u.draws + if draw { 1 } else { 0 },
                ..u
            });
        }
    };
    match winner_user_id {
        Some(w) => {
            let loser = if w == game.white_user_id {
                game.black_user_id
            } else {
                game.white_user_id
            };
            update(w, true, false);
            update(loser, false, false);
        }
        None => {
            update(game.white_user_id, false, true);
            update(game.black_user_id, false, true);
        }
    }
}
