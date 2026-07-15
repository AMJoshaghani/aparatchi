use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::PathBuf;

use crate::models::{Entry, EntryKind, Episode};

// Where the app keeps its data: the sqlite database lives directly here,
// and generated poster/thumbnail images live one level down in a
// `posters/` subfolder.
pub fn data_dir() -> PathBuf {
    directories::ProjectDirs::from("com", "local", "aparatchi")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn open() -> Result<Connection> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    let db_path = dir.join("movies.db");
    let conn = Connection::open(db_path)?;
    conn.execute_batch(
        "
        PRAGMA foreign_keys = ON;
        CREATE TABLE IF NOT EXISTS entries (
            id               INTEGER PRIMARY KEY AUTOINCREMENT,
            title            TEXT NOT NULL,
            kind             TEXT NOT NULL,
            description      TEXT NOT NULL DEFAULT '',
            link_or_path     TEXT NOT NULL DEFAULT '',
            resume_position  INTEGER NOT NULL DEFAULT 0,
            duration         INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS episodes (
            id               INTEGER PRIMARY KEY AUTOINCREMENT,
            entry_id         INTEGER NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
            season           INTEGER NOT NULL DEFAULT 1,
            episode          INTEGER NOT NULL DEFAULT 1,
            title            TEXT NOT NULL DEFAULT '',
            description      TEXT NOT NULL DEFAULT '',
            link_or_path     TEXT NOT NULL DEFAULT '',
            resume_position  INTEGER NOT NULL DEFAULT 0,
            duration         INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS settings (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS season_patterns (
            entry_id     INTEGER NOT NULL,
            season       INTEGER NOT NULL,
            pattern      TEXT NOT NULL,
            season_width INTEGER NOT NULL DEFAULT 2,
            ep_width     INTEGER NOT NULL DEFAULT 2,
            PRIMARY KEY (entry_id, season)
        );
        ",
    )?;
    // Databases from older versions of the app won't have this column yet,
    // so we add it if it's missing. That way existing installs just upgrade
    // in place instead of crashing on startup.
    ensure_column(&conn, "entries", "finished", "finished INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(&conn, "episodes", "finished", "finished INTEGER NOT NULL DEFAULT 0")?;
    // Needed for the "Recently watched" sort option.
    ensure_column(&conn, "entries", "last_watched_at", "last_watched_at INTEGER NOT NULL DEFAULT 0")?;
    Ok(conn)
}

fn ensure_column(conn: &Connection, table: &str, column: &str, add_column_ddl: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let existing = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    if !existing.iter().any(|c| c == column) {
        conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {add_column_ddl}"), [])?;
    }
    Ok(())
}

// Whatever the user last played - lets the landing page offer a one-click
// "Resume" even after the app's been restarted.
pub struct LastPlayed {
    pub kind: EntryKind,
    pub entry_id: i64,
    pub episode_id: Option<i64>,
}

pub fn get_last_played(conn: &Connection) -> Result<Option<LastPlayed>> {
    let value: Option<String> = conn
        .query_row("SELECT value FROM settings WHERE key = 'last_played'", [], |r| r.get(0))
        .optional()?;
    Ok(value.and_then(|v| parse_last_played(&v)))
}

fn parse_last_played(v: &str) -> Option<LastPlayed> {
    let parts: Vec<&str> = v.splitn(3, ':').collect();
    if parts.len() != 3 {
        return None;
    }
    let kind = EntryKind::from_str(parts[0]);
    let entry_id: i64 = parts[1].parse().ok()?;
    let episode_id: Option<i64> = if parts[2].is_empty() { None } else { parts[2].parse().ok() };
    Some(LastPlayed { kind, entry_id, episode_id })
}

pub fn set_last_played(conn: &Connection, kind: EntryKind, entry_id: i64, episode_id: Option<i64>) -> Result<()> {
    let value = format!(
        "{}:{}:{}",
        kind.as_str(),
        entry_id,
        episode_id.map(|x| x.to_string()).unwrap_or_default()
    );
    conn.execute(
        "INSERT INTO settings (key, value) VALUES ('last_played', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![value],
    )?;
    Ok(())
}

pub fn clear_last_played(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM settings WHERE key = 'last_played'", [])?;
    Ok(())
}

// The VLC path the user set in Settings, if any. `None` just means
// "nobody's configured this yet" - whoever calls this should fall back to
// auto-detection in that case.
pub fn get_vlc_path(conn: &Connection) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM settings WHERE key = 'vlc_path'", [], |r| r.get(0))
        .optional()?)
}

pub fn set_vlc_path(conn: &Connection, path: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES ('vlc_path', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![path],
    )?;
    Ok(())
}

// The subtitle language the user prefers ("eng", "fas", whatever), which
// gets passed to VLC via `--sub-language`. Empty/`None` just means we
// don't ask VLC for anything specific.
pub fn get_subtitle_lang(conn: &Connection) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM settings WHERE key = 'subtitle_lang'", [], |r| r.get(0))
        .optional()?)
}

pub fn set_subtitle_lang(conn: &Connection, lang: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES ('subtitle_lang', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![lang],
    )?;
    Ok(())
}

const ENTRY_COLUMNS: &str = "id, title, kind, description, link_or_path, resume_position, duration, finished, last_watched_at";
const EPISODE_COLUMNS: &str =
    "id, entry_id, season, episode, title, description, link_or_path, resume_position, duration, finished";

pub fn list_entries(conn: &Connection, kind: EntryKind) -> Result<Vec<Entry>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ENTRY_COLUMNS} FROM entries WHERE kind = ?1 ORDER BY title COLLATE NOCASE"
    ))?;
    let rows = stmt.query_map(params![kind.as_str()], row_to_entry)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn get_entry(conn: &Connection, id: i64) -> Result<Entry> {
    let mut stmt = conn.prepare(&format!("SELECT {ENTRY_COLUMNS} FROM entries WHERE id = ?1"))?;
    Ok(stmt.query_row(params![id], row_to_entry)?)
}

fn row_to_entry(row: &rusqlite::Row) -> rusqlite::Result<Entry> {
    Ok(Entry {
        id: row.get(0)?,
        title: row.get(1)?,
        kind: EntryKind::from_str(&row.get::<_, String>(2)?),
        description: row.get(3)?,
        link_or_path: row.get(4)?,
        resume_position: row.get(5)?,
        duration: row.get(6)?,
        finished: row.get::<_, i64>(7)? != 0,
        last_watched_at: row.get(8)?,
    })
}

// Stamps this entry with "right now" as its last-watched time, for the
// "Recently watched" sort. We lean on SQLite's own clock here so there's no
// need to pull in a separate time crate just for this.
pub fn touch_last_watched(conn: &Connection, id: i64) -> Result<()> {
    conn.execute(
        "UPDATE entries SET last_watched_at = CAST(strftime('%s','now') AS INTEGER) WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

pub fn insert_entry(
    conn: &Connection,
    title: &str,
    kind: EntryKind,
    description: &str,
    link_or_path: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO entries (title, kind, description, link_or_path) VALUES (?1, ?2, ?3, ?4)",
        params![title, kind.as_str(), description, link_or_path],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn update_entry(
    conn: &Connection,
    id: i64,
    title: &str,
    description: &str,
    link_or_path: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE entries SET title = ?2, description = ?3, link_or_path = ?4 WHERE id = ?1",
        params![id, title, description, link_or_path],
    )?;
    Ok(())
}

pub fn update_entry_resume(conn: &Connection, id: i64, position: i64, duration: i64, finished: bool) -> Result<()> {
    conn.execute(
        "UPDATE entries SET resume_position = ?2, duration = ?3, finished = ?4 WHERE id = ?1",
        params![id, position, duration, finished as i64],
    )?;
    Ok(())
}

// Deletes an entry - movie or series. If it's a series, its episodes get
// swept away automatically thanks to the `ON DELETE CASCADE` foreign key,
// so there's nothing extra to clean up here.
pub fn delete_entry(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM entries WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn list_episodes(conn: &Connection, entry_id: i64) -> Result<Vec<Episode>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {EPISODE_COLUMNS} FROM episodes WHERE entry_id = ?1 ORDER BY season, episode"
    ))?;
    let rows = stmt.query_map(params![entry_id], row_to_episode)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn get_episode(conn: &Connection, id: i64) -> Result<Episode> {
    let mut stmt = conn.prepare(&format!("SELECT {EPISODE_COLUMNS} FROM episodes WHERE id = ?1"))?;
    Ok(stmt.query_row(params![id], row_to_episode)?)
}

// The next episode after the given one, in season/episode order. This is
// what lets us auto-advance to the "current" episode once one gets marked
// finished.
pub fn next_episode(conn: &Connection, entry_id: i64, after_season: i32, after_episode: i32) -> Result<Option<Episode>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {EPISODE_COLUMNS} FROM episodes
         WHERE entry_id = ?1 AND (season > ?2 OR (season = ?2 AND episode > ?3))
         ORDER BY season, episode LIMIT 1"
    ))?;
    Ok(stmt
        .query_row(params![entry_id, after_season, after_episode], row_to_episode)
        .optional()?)
}

// The earliest episode that isn't finished yet, in season/episode order.
pub fn first_unfinished_episode(conn: &Connection, entry_id: i64) -> Result<Option<Episode>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {EPISODE_COLUMNS} FROM episodes WHERE entry_id = ?1 AND finished = 0 ORDER BY season, episode LIMIT 1"
    ))?;
    Ok(stmt.query_row(params![entry_id], row_to_episode).optional()?)
}

// Whatever the last episode is, in season/episode order - finished or not.
pub fn last_episode(conn: &Connection, entry_id: i64) -> Result<Option<Episode>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {EPISODE_COLUMNS} FROM episodes WHERE entry_id = ?1 ORDER BY season DESC, episode DESC LIMIT 1"
    ))?;
    Ok(stmt.query_row(params![entry_id], row_to_episode).optional()?)
}

// Figures out where a "Resume"/"Continue watching" action should actually
// land: the first unfinished episode, or if everything's already been
// watched (or there's just one episode total), the last one.
pub fn current_episode(conn: &Connection, entry_id: i64) -> Result<Option<Episode>> {
    if let Some(ep) = first_unfinished_episode(conn, entry_id)? {
        return Ok(Some(ep));
    }
    last_episode(conn, entry_id)
}

fn row_to_episode(row: &rusqlite::Row) -> rusqlite::Result<Episode> {
    Ok(Episode {
        id: row.get(0)?,
        entry_id: row.get(1)?,
        season: row.get(2)?,
        episode: row.get(3)?,
        title: row.get(4)?,
        description: row.get(5)?,
        link_or_path: row.get(6)?,
        resume_position: row.get(7)?,
        duration: row.get(8)?,
        finished: row.get::<_, i64>(9)? != 0,
    })
}

pub fn insert_episode(
    conn: &Connection,
    entry_id: i64,
    season: i32,
    episode: i32,
    title: &str,
    description: &str,
    link_or_path: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO episodes (entry_id, season, episode, title, description, link_or_path) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![entry_id, season, episode, title, description, link_or_path],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn update_episode(conn: &Connection, id: i64, title: &str, description: &str, link_or_path: &str) -> Result<()> {
    conn.execute(
        "UPDATE episodes SET title = ?2, description = ?3, link_or_path = ?4 WHERE id = ?1",
        params![id, title, description, link_or_path],
    )?;
    Ok(())
}

pub fn update_episode_resume(conn: &Connection, id: i64, position: i64, duration: i64, finished: bool) -> Result<()> {
    conn.execute(
        "UPDATE episodes SET resume_position = ?2, duration = ?3, finished = ?4 WHERE id = ?1",
        params![id, position, duration, finished as i64],
    )?;
    Ok(())
}

pub fn delete_episode(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM episodes WHERE id = ?1", params![id])?;
    Ok(())
}

// Wipes out an entire season - every episode in it - from a series.
pub fn delete_season(conn: &Connection, entry_id: i64, season: i32) -> Result<()> {
    conn.execute("DELETE FROM episodes WHERE entry_id = ?1 AND season = ?2", params![entry_id, season])?;
    conn.execute(
        "DELETE FROM season_patterns WHERE entry_id = ?1 AND season = ?2",
        params![entry_id, season],
    )?;
    Ok(())
}

// Remembers the link pattern (and digit widths) that was used to
// auto-detect a season's episodes. That way "Re-probe" can go find newly
// added episodes later without making the user type the pattern out again.
pub fn set_season_pattern(
    conn: &Connection,
    entry_id: i64,
    season: i32,
    pattern: &str,
    season_width: i32,
    ep_width: i32,
) -> Result<()> {
    conn.execute(
        "INSERT INTO season_patterns (entry_id, season, pattern, season_width, ep_width) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(entry_id, season) DO UPDATE SET
            pattern = excluded.pattern, season_width = excluded.season_width, ep_width = excluded.ep_width",
        params![entry_id, season, pattern, season_width, ep_width],
    )?;
    Ok(())
}

// Whatever pattern/season-width/episode-width was saved for this season,
// if anything. `None` just means the season was added by hand, so there's
// no pattern to re-probe with.
pub fn get_season_pattern(conn: &Connection, entry_id: i64, season: i32) -> Result<Option<(String, i32, i32)>> {
    Ok(conn
        .query_row(
            "SELECT pattern, season_width, ep_width FROM season_patterns WHERE entry_id = ?1 AND season = ?2",
            params![entry_id, season],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?)
}
