# Chess — SpacetimeDB + Rust CLI

A multiplayer chess game built on SpacetimeDB. Features account-based auth, matchmaking, spectator mode, move history, and PGN export.

## Project Structure

```
.
├── spacetimedb/src/lib.rs   ← Server module
├── spacetimedb/Cargo.toml   ← Server dependencies
├── src/main.rs              ← Client module
└── Cargo.toml               ← Client dependencies
```

---

## Setup

### 1. Publish the server module

```bash
spacetime publish chess-db --module-path ./spacetimedb
```

To wipe and republish after schema changes:
```bash
spacetime publish chess-db --clear-database -y --module-path ./spacetimedb
```

### 2. Generate client bindings

```bash
spacetime generate --lang rust \
  --out-dir ./src/module_bindings \
  --module-path ./spacetimedb
```

This creates `src/module_bindings/` — **do not edit these files manually**. Re-run this after every schema change.

### 3. Run the client

```bash
cargo run
```

Override host/database with env vars:
```bash
SPACETIMEDB_HOST=http://localhost:3000 SPACETIMEDB_DB_NAME=chess-db cargo run
```

---

## Quick Start (two terminals)

On startup you'll see the auth prompt. Create an account or log in before doing anything else.

**Terminal 1:**
```
auth> register Alice
  Password (min 8 chars): ········
  Confirm password: ········
✓ Account created and logged in as 'Alice'.

Alice> join
Joined matchmaking lobby…
```

**Terminal 2:**
```
auth> register Bob
  Password (min 8 chars): ········
  Confirm password: ········
✓ Account created and logged in as 'Bob'.

Bob> join
🎮 Game #1 started! You are White ♔
```

A game starts automatically when two players are in the lobby. Colors are assigned randomly.

**Making moves** (squares use standard algebraic notation `a1`–`h8`):
```
Alice> move e2 e4
Alice> move g1 f3
Alice> move e7 e8 Q     ← promotion
```

---

## Auth Commands

| Command | Description |
|---|---|
| `login <username>` | Log in to an existing account (prompts for password) |
| `register <username>` | Create a new account (prompts for password + confirm) |
| `logout` | Log out and return to the auth prompt |
| `passwd` | Change your password |
| `whoami` | Show your username, user ID, and stats |

---

## Game Commands

| Command | Description |
|---|---|
| `lobby` | Show players waiting for a match |
| `join` | Join matchmaking — starts a game immediately if an opponent is waiting |
| `leave-lobby` | Leave the lobby |
| `games` | List all active games |
| `game <id>` | Set your active game (needed before `move`, `resign`, etc.) |
| `board [id]` | Print the current board |
| `move <from> <to> [piece]` | Make a move, e.g. `move e2 e4` or `move e7 e8 Q` |
| `resign` | Resign the current game |
| `draw` | Offer a draw — accepted automatically if the opponent has also offered |
| `history [id]` | Show the move list for a game |
| `pgn [id]` | Export the game in PGN format |
| `spectate <id>` | Watch a game live |
| `unspectate` | Stop watching |
| `spectators [id]` | List who is spectating a game |
| `chat <message>` | Send a message (in-game, or lobby chat when no game is active) |
| `leaderboard` | Show player rankings by wins |
| `quit` | Exit |
