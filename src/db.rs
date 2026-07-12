use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

use crate::models::{Entry, EntryKind, Episode};

pub fn open() -> Result<Connection> {
    let dir = directories::ProjectDirs::from("com", "local", "aparatchi")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
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
        ",
    )?;
    // Older databases predate the "finished" column. add it if missing so
    // existing installs upgrade in place instead of failing.
    ensure_column(&conn, "entries", "finished", "finished INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(&conn, "episodes", "finished", "finished INTEGER NOT NULL DEFAULT 0")?;
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

/// What the user last played, so the UI can offer a one-click "Resume" on
/// the empty landing page even after restarting the app.
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

/// Path to the VLC binary the user configured in Settings. `None` means
/// "not configured" -> callers should fall back to auto-detection.
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

const ENTRY_COLUMNS: &str = "id, title, kind, description, link_or_path, resume_position, duration, finished";
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
    })
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

/// Deletes an entry (movie or series). Episodes belonging to a series are
/// removed automatically via the `ON DELETE CASCADE` foreign key.
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

/// First episode (in season/episode order) strictly after the given one.
/// Used to auto-advance the "current" episode once one is marked finished.
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

/// The first not-yet-finished episode, in season/episode order.
pub fn first_unfinished_episode(conn: &Connection, entry_id: i64) -> Result<Option<Episode>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {EPISODE_COLUMNS} FROM episodes WHERE entry_id = ?1 AND finished = 0 ORDER BY season, episode LIMIT 1"
    ))?;
    Ok(stmt.query_row(params![entry_id], row_to_episode).optional()?)
}

/// The very last episode (in season/episode order), regardless of status.
pub fn last_episode(conn: &Connection, entry_id: i64) -> Result<Option<Episode>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {EPISODE_COLUMNS} FROM episodes WHERE entry_id = ?1 ORDER BY season DESC, episode DESC LIMIT 1"
    ))?;
    Ok(stmt.query_row(params![entry_id], row_to_episode).optional()?)
}

/// The episode a "Resume"/"Continue watching" action should land on: the
/// first unfinished one, or the last episode if everything's been finished
/// (or there's only one to begin with).
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

/// Deletes an entire season (all its episodes) from a series.
pub fn delete_season(conn: &Connection, entry_id: i64, season: i32) -> Result<()> {
    conn.execute("DELETE FROM episodes WHERE entry_id = ?1 AND season = ?2", params![entry_id, season])?;
    Ok(())
}
