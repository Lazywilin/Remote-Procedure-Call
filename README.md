# gts-rpc-gui

A small desktop app (Rust + egui) to browse Discord's list of detectable games and show one as your custom Discord Rich Presence status, with a full status editor.

**Display only.** It uses Discord's local Rich Presence IPC, the same mechanism as any custom RPC tool. It does **not** send play-time or analytics data to Discord, and it does not change your badges or real play time.

## Features

**Game browser**
- Full detectable-games list from Discord, with cover art
- Search by name or alias, and a genre filter
- Tabs: All Games, Favorites, Detected (games currently running on your PC)
- Favorites saved between runs
- Click a cover to open the game's page in Discord's app directory

**Status editor**
- Activity type: Playing, Listening to, Watching, Competing in
- Two custom text lines, with a live preview card that looks like Discord's activity card
- Edits apply to your live status automatically (can be switched off in Settings)
- Timer modes: Elapsed ("started X hours ago"), Countdown ("ends in X hours") or Hidden
- Party size (for example 2 of 5)
- Use the game's icon as the large image, with hover text
- Optional small badge image (a direct https link to a PNG you host), with hover text
- Platform tag: adds plain text like `on PlayStation 5` to line 2 (PlayStation 5/4, Xbox, PC)
- **PlayStation look** button: PS5 tag, game icon, a white PlayStation logo badge (`assets/playstation-logo.png`) and an elapsed timer in one click
- Up to two custom buttons (https links)
- Auto store buttons: Steam, PlayStation Store and Xbox Store links built for the game you pick

**Presets and look**
- Save a game plus its style as a preset, apply it in one click, or mark one with ⚡ to apply on launch
- Monochrome dark or light theme
- `--minimized` launch flag, handy together with a ⚡ preset

## Requirements

- [Rust](https://rustup.rs) (stable)
- The **Discord desktop app**, running, on the same machine
- In Discord: Settings → Activity Privacy → enable "Share your detected activities with others", otherwise nobody can see your status

## Build and run

```bash
git clone https://github.com/Lazywilin/Remote-Procedure-Call
cd Remote-Procedure-Call
cd "Discord RPC Rust"
cargo run --release
```

Start minimized: `cargo run --release -- --minimized`

Run the tests: `cargo test`

## Usage

1. Start Discord, then start the app.
2. Browse or search for a game. Use the tabs and genre dropdown to narrow it down.
3. Click **Spoof** on a game. Your status appears within a few seconds.
4. Style it in the panel on the right. It has four tabs: **Status** (lines, timer, party), **Extras** (platform tag, images, buttons), **Presets** and **Settings**. Changes apply to your live status within a second.
5. Click **Stop** (or close the app) to clear it.
6. To reuse a setup, name it and click **Save** under Presets. Click **Apply** later, or ⚡ to apply it on launch.

Settings are stored in a `gts-rpc` folder inside your config directory (`config.json` for presets and theme, `favorites.txt` for favorites).

## Limitations

- Discord shows one status per connection, so spoofing a new game replaces the current one.
- Works with the desktop client only. Web Discord has no local IPC socket.
- Linux Flatpak/Snap Discord may block the IPC socket until you expose it to the sandbox.
- **The "Playing on PlayStation" header can't be set.** Discord adds that header and logo itself for linked console accounts. This app can only add plain text. The bundled white PlayStation logo is shown as a small badge image instead, and you can point the small badge field at your own PNG for a different logo. The badge loads from this repo's raw GitHub link (`PS_BADGE_URL` in `main.rs`), so it appears only after the repo is public and the path matches.
- **Buttons are text only.** Discord does not allow logos or icons on Rich Presence buttons.
- **Store buttons are mostly search links.** Discord's game list has no PlayStation or Xbox product IDs, so those buttons open a store search for the game's name. Steam links straight to the store page only when Discord's data includes a Steam ID for the game. Otherwise it is a search link too. Games that are not released yet may have no store page at all.
- Other people can see your buttons, but you can't see them on your own profile.
- Some Discord versions ignore the activity type and show "Playing".
- The large-image icon is passed to Discord as a URL and may not render on every client. If it doesn't show, turn the option off.
- The Detected tab matches running process names against Discord's executable list, so it may miss some games.
- There is no "only games I haven't played" filter, since that needs your account session.

## Disclaimer

Not affiliated with or endorsed by Discord. Showing a game you aren't playing is a cosmetic status only. Use it responsibly and in line with [Discord's Terms of Service](https://discord.com/safety/platform-manipulation-policy-explainer).

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).

## Credits

Based on the [gameTimeSpoof plugin](https://github.com/LostMorality/cordmuffin/tree/main/custom-creations/gameTimeSpoof) from LostMorality/cordmuffin (GPL-3.0-or-later). This is a Rust rewrite of its game browser and status features. The play-time and badge code is not included.
