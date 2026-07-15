# Aparatchi (Movie & Series Manager)

A desktop app (Rust + Slint) to track the movies/series you watch, play them in
VLC, and resume exactly where you left off.

## Screenshot
![main page](https://s6.imgcdn.dev/YFdaFV.png)
![series page](https://s6.imgcdn.dev/YFd70o.png)
![adding page](https://s6.imgcdn.dev/YFBMXd.png)


## Features

- **Detail pane**: shows description, the link/file path, and (for series) a
  selectable list of episodes. **Play** and **Resume** functionalities. powered by SQLite (stores the last known playback position) and
  VLC (reports actual position).
- **Batch episode generation**: for a series, give a URL/path pattern with
  `*` standing in for the episode number and `#` as the season number (e.g.
  `http://example.com/S#/Movie.S#E*.mkv`), a point of start (like `1`)
  and an auto-detected range for episodes and seasona. The app expands that into one row per episode and verifies each one (HTTP HEAD request for URLs, filesystem
  check for local paths) before saving.
- **Edit** button on each entry's page to update the title/description/link
  (or, when a specific episode is selected, that episode's title/link).
- **Search** search is available on all series & movies with recently added, recently watched and "A-Z" filters.
- **Bulk Management** bulk of episodes and seasons can be managed with ease, and re-probing (searching for episodes and seasons in URL) is available.

## How playback and resume works

Using VLC accounts for the multiplatform fragility across Linux/Windows/Mac and enables powerful customizations (not to mention that VLC really rocks and this is enough reason to use it). The app launches the real `vlc` binary as a child process with its built-in HTTP interface turned on (a random local port + password each run), and:

- **Play** launches VLC at position 0.
- **Resume** launches VLC with `--start-time=<seconds>` set to the last saved
  position.
- While VLC is running, a background thread polls `status.json` over that
  HTTP interface every ~1.5s to track the current time and length.
- When VLC exits, the last known position is written to SQLite (per-movie or
  per-episode). If playback reached ≥90% of the file's length, the resume
  point is cleared instead (treated as "finished").

This means resume works even if the user just closes the VLC window. No
special shutdown handling required.

## Requirements to build & run

- Rust (`rustup` is easiest)
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

## Disclaimer!
This project does not support or endorse any kind of piracy, and the developer doesn't mean for it to be used as such.
