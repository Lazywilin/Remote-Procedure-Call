# gts-rpc-gui

A small desktop app (Rust + egui) that lets you browse Discord's list of detectable games and show one as your Discord status ("Playing …"), with a custom elapsed-time timer.

**Display only.** It uses Discord's local Rich Presence IPC, the same mechanism as any custom RPC tool. It does **not** send play-time or analytics data to Discord, and it does not change your badges or real play time.

## Features

- Full detectable-games list from Discord, with cover art
- Search by name or alias, and a genre filter
- Tabs: All Games, Favorites, Detected (games currently running on your PC)
- Favorites saved between runs
- One-click Spoof / Stop, with the active game pinned to the top
- Set how long you appear to have been playing (hours) and an optional details line
- Click a cover to open the game's page in Discord's app directory

## Requirements

- [Rust](https://rustup.rs) (stable)
- The **Discord desktop app**, running, on the same machine
- In Discord: Settings → Activity Privacy → enable "Share your detected activities with others", otherwise nobody can see your status

## Build and run

```bash
git clone https://github.com/<you>/gts-rpc-gui
cd gts-rpc-gui
cargo run --release
```

## Usage

1. Start Discord, then start the app.
2. Browse or search for a game. Use the tabs and genre dropdown to narrow it down.
3. Optionally set **Playing for (hours)** and a **Details** line.
4. Click **Spoof** on a game. Your status updates within a few seconds.
5. Click **Stop** (or close the app) to clear it.
6. Click the star to favorite a game. Favorites are stored in `gts-rpc/favorites.txt` inside your config folder.

## Limitations

- Discord shows one status per connection, so spoofing a new game replaces the current one.
- Works with the desktop client only. Web Discord has no local IPC socket.
- Linux Flatpak/Snap Discord may block the IPC socket until you expose it to the sandbox.
- Detected tab matches running process names against Discord's executable list, so it may miss some games.
- There is no "only games I haven't played" filter, since that needs your account session.

## Disclaimer

Not affiliated with or endorsed by Discord. Showing a game you aren't playing is a cosmetic status only. Use it responsibly and in line with Discord's Terms of Service.

## License

GPL-3.0-or-later. This project's feature set is derived from the GameTimeSpoof Equicord plugin (GPL-3.0-or-later), with the analytics/badge code removed. Keep that credit and license when you publish.
Based on the "gameTimeSpoof" plugin from LostMorality/cordmuffin.
