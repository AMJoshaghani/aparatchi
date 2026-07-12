# Aparatchi (Movie & Series Manager)

A desktop app (Rust + Slint) to track the movies/series you watch, play them in
VLC, and resume exactly where you left off.

## Screenshot
![main page](https://s6.imgcdn.dev/YFB6AM.png)
![series page](https://s6.imgcdn.dev/YFBwy0.png)
![adding page](https://s6.imgcdn.dev/YFBMXd.png)


## Features

- **Detail pane**: shows description, the link/file path, and (for series) a
  selectable list of episodes. **Play** and **Resume** buttons at the bottom
  right — powered by SQLite (stores the last known playback position) and
  VLC (reports actual position).
- **Batch episode generation**: for a series, give a URL/path pattern with
  `*` standing in for the episode number and `#` as the season number (e.g.
  `http://example.com/S#/Movie.S#E*.mkv`), a point of start (like `1`)
  and an auto-detected range for episodes and seasona. The app expands that into one row per episode and verifies each one (HTTP HEAD request for URLs, filesystem
  check for local paths) before saving.
- **Edit** button on each entry's page to update the title/description/link
  (or, when a specific episode is selected, that episode's title/link).

## How playback + resume actually works

Slint doesn't have a built-in way to embed a video surface, and hand-rolling
libvlc window-handle embedding is fragile across Linux/macOS/Windows. Instead,
the app launches the real `vlc` binary as a child process with its built-in
HTTP interface turned on (a random local port + password each run), and:

- **Play** launches VLC at position 0.
- **Resume** launches VLC with `--start-time=<seconds>` set to the last saved
  position.
- While VLC is running, a background thread polls `status.json` over that
  HTTP interface every ~1.5s to track the current time and length.
- When VLC exits, the last known position is written to SQLite (per-movie or
  per-episode). If playback reached ≥90% of the file's length, the resume
  point is cleared instead (treated as "finished").

This means resume works even if the user just closes the VLC window — no
special shutdown handling required.

## Requirements to build & run

- Rust (stable, recent — `rustup` is easiest)
- VLC installed and it's binary recognizable. You can also manually change binary path in "Settings":
  - Linux: Install via preffered package manager (`apt`, `pacman`, etc.). Uses installed binary after auto-detection. Usually `/usr/bin/vlc`
  - macOS: Install via `brew install --cask vlc`. Would be the same spot as Linux.
  - Windows: install [VLC](https://www.videolan.org/vlc/), and the auto-detection would find it. Otherwise you can always manually select it.

SQLite is bundled via `rusqlite`'s `bundled` feature, so no separate SQLite
install is needed.

## Build & run

```bash
cargo build --release
cargo run --release
```

The database is created automatically at your OS's standard app-data
location (e.g. `~/.local/share/aparatchi/movies.db` on Linux,
`~/Library/Application Support/com.local.aparatchi/movies.db` on macOS,
`%APPDATA%\local\aparatchi\data\movies.db` on Windows).

## Project layout

```
Cargo.toml
build.rs                # compiles ui/app.slint
ui/app.slint            # all UI: sidebar, detail pane, Add/Edit dialogs
ui/icon.png             # icon used for program window
src/main.rs             # wires UI callbacks to db/vlc logic
src/db.rs               # SQLite schema + CRUD (rusqlite)
src/vlc.rs              # launches VLC, polls its HTTP interface for status
src/pattern.rs          # "*" pattern expansion + link/file verification
src/models.rs           # Entry / Episode / EntryKind types
```

## Notes / things you may want to tweak

- Episode "titles" default to `Episode <n>`; edit any episode's title from
  the entry page (select it, then hit Edit).
- Verification of `http(s)://` links does a `HEAD` request with a 6s
  timeout; some servers reject `HEAD`, so a `405` response is still counted
  as "reachable". Local file paths are checked with a plain existence check.
- The "finished" threshold (90% watched → resume cleared) is a constant in
  `start_playback` in `src/main.rs` if you'd like to change it.

## Disclaimer!
This project does not support or endorse any kind of piracy, and the developer doesn't mean for it to be used as such.
