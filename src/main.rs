mod db;
mod models;
mod pattern;
mod vlc;

use models::{EntryKind, Episode};
use rusqlite::Connection;
use slint::{ComponentHandle, ModelRc, VecModel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

slint::include_modules!();

/// A playback session is considered "finished" once it has played through
/// this fraction of the total runtime.
const FINISHED_THRESHOLD: f64 = 0.90;

/// The VLC binary to launch: whatever's configured in Settings, or a
/// best-effort auto-detected path, or (as an absolute last resort) just the
/// bare binary name and let the OS resolve it via PATH.
fn resolve_vlc_path(conn: &Connection) -> String {
    if let Ok(Some(p)) = db::get_vlc_path(conn) {
        if !p.trim().is_empty() {
            return p;
        }
    }
    vlc::detect_vlc().unwrap_or_else(|| vlc::default_binary_name().to_string())
}

fn format_duration(total_seconds: i64) -> String {
    let total_seconds = total_seconds.max(0);
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// "12:34 / 45:00" style progress readout, or empty if we don't know the
/// runtime yet (nothing's been played, or VLC never reported a length).
fn progress_string(resume_position: i64, duration: i64) -> String {
    if duration > 0 {
        format!("{} / {}", format_duration(resume_position), format_duration(duration))
    } else {
        String::new()
    }
}

#[derive(Clone)]
enum Selection {
    None,
    Movie(i64),
    Series { entry_id: i64, episode_id: Option<i64> },
}

#[derive(Clone)]
enum DeleteTarget {
    Entry(i64),
    Episode(i64),
    Season(i64, i32),
}

#[derive(Clone)]
enum EditTarget {
    Entry(i64),
    Episode(i64),
}

#[derive(Clone)]
enum PlaybackTarget {
    Entry(i64, String),
    Episode(i64, String),
}

impl PlaybackTarget {
    fn link(&self) -> &str {
        match self {
            PlaybackTarget::Entry(_, l) | PlaybackTarget::Episode(_, l) => l,
        }
    }
    fn persist(&self, conn: &Connection, pos: i64, dur: i64, finished: bool) -> anyhow::Result<()> {
        match self {
            PlaybackTarget::Entry(id, _) => db::update_entry_resume(conn, *id, pos, dur, finished),
            PlaybackTarget::Episode(id, _) => db::update_episode_resume(conn, *id, pos, dur, finished),
        }
    }
}

struct AppState {
    conn: Mutex<Connection>,
    selected: Mutex<Selection>,
    vlc: Mutex<Option<vlc::VlcSession>>,
    pending_delete: Mutex<Option<DeleteTarget>>,
    pending_edit: Mutex<Option<EditTarget>>,
}

fn main() -> anyhow::Result<()> {
    let conn = db::open()?;
    let app = Arc::new(AppState {
        conn: Mutex::new(conn),
        selected: Mutex::new(Selection::None),
        vlc: Mutex::new(None),
        pending_delete: Mutex::new(None),
        pending_edit: Mutex::new(None),
    });

    let ui = MainWindow::new()?;
    refresh_sidebar(&app, &ui);
    refresh_last_played(&app, &ui);

    let backend = ui.global::<Backend>();

    // ---- sidebar section toggles ----
    {
        let ui_weak = ui.as_weak();
        backend.on_toggle_movies(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let b = ui.global::<Backend>();
                let v = b.get_movies_expanded();
                b.set_movies_expanded(!v);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        backend.on_toggle_series(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let b = ui.global::<Backend>();
                let v = b.get_series_expanded();
                b.set_series_expanded(!v);
            }
        });
    }

    // ---- selection ----
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_select_entry(move |id, kind| {
            if let Some(ui) = ui_weak.upgrade() {
                select_entry(&app, &ui, id as i64, &kind);
            }
        });
    }
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_select_episode(move |id| {
            if let Some(ui) = ui_weak.upgrade() {
                select_episode(&app, &ui, id as i64);
            }
        });
    }

    // ---- play / resume ----
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_play(move || {
            if let Some(ui) = ui_weak.upgrade() {
                do_play(&app, &ui, false);
            }
        });
    }
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_resume(move || {
            if let Some(ui) = ui_weak.upgrade() {
                do_play(&app, &ui, true);
            }
        });
    }
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_resume_entry(move || {
            if let Some(ui) = ui_weak.upgrade() {
                resume_entry(&app, &ui);
            }
        });
    }

    // ---- add dialog (new movie/series) ----
    {
        let ui_weak = ui.as_weak();
        backend.on_open_add_dialog(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let b = ui.global::<Backend>();
                b.set_form_title("".into());
                b.set_form_description("".into());
                b.set_form_link("".into());
                b.set_form_type("movie".into());
                b.set_pattern_link("".into());
                b.set_pattern_season_start("1".into());
                b.set_pattern_season_width("2".into());
                b.set_pattern_start("1".into());
                b.set_pattern_episode_width("2".into());
                b.set_status_text("".into());
                b.set_add_dialog_open(true);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        backend.on_close_add_dialog(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.global::<Backend>().set_add_dialog_open(false);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        backend.on_browse_file(move || {
            if let Some(ui) = ui_weak.upgrade() {
                if let Some(path) = rfd::FileDialog::new().pick_file() {
                    ui.global::<Backend>().set_form_link(path.display().to_string().into());
                }
            }
        });
    }
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_submit_add(move || {
            if let Some(ui) = ui_weak.upgrade() {
                submit_add(&app, &ui);
            }
        });
    }

    // ---- edit dialog (entry title, or a single episode) ----
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_open_edit_entry_dialog(move || {
            if let Some(ui) = ui_weak.upgrade() {
                open_edit_entry_dialog(&app, &ui);
            }
        });
    }
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_open_edit_episode_dialog(move |id| {
            if let Some(ui) = ui_weak.upgrade() {
                open_edit_episode_dialog(&app, &ui, id as i64);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        backend.on_close_edit_dialog(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.global::<Backend>().set_edit_dialog_open(false);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        backend.on_browse_edit_file(move || {
            if let Some(ui) = ui_weak.upgrade() {
                if let Some(path) = rfd::FileDialog::new().pick_file() {
                    ui.global::<Backend>().set_edit_link(path.display().to_string().into());
                }
            }
        });
    }
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_submit_edit(move || {
            if let Some(ui) = ui_weak.upgrade() {
                submit_edit(&app, &ui);
            }
        });
    }

    // ---- delete confirmation (entry / episode / season) ----
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_request_delete_entry(move || {
            if let Some(ui) = ui_weak.upgrade() {
                request_delete_entry(&app, &ui);
            }
        });
    }
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_request_delete_episode(move |id| {
            if let Some(ui) = ui_weak.upgrade() {
                request_delete_episode(&app, &ui, id as i64);
            }
        });
    }
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_request_delete_season(move |season| {
            if let Some(ui) = ui_weak.upgrade() {
                request_delete_season(&app, &ui, season);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        backend.on_cancel_delete(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.global::<Backend>().set_confirm_dialog_open(false);
            }
        });
    }
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_confirm_delete(move || {
            if let Some(ui) = ui_weak.upgrade() {
                confirm_delete(&app, &ui);
            }
        });
    }

    // ---- add episode dialog ----
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_open_add_episode_dialog(move |season| {
            if let Some(ui) = ui_weak.upgrade() {
                open_add_episode_dialog(&app, &ui, season);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        backend.on_close_add_episode_dialog(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.global::<Backend>().set_add_episode_dialog_open(false);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        backend.on_browse_add_episode_file(move || {
            if let Some(ui) = ui_weak.upgrade() {
                if let Some(path) = rfd::FileDialog::new().pick_file() {
                    ui.global::<Backend>().set_add_episode_link(path.display().to_string().into());
                }
            }
        });
    }
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_submit_add_episode(move || {
            if let Some(ui) = ui_weak.upgrade() {
                submit_add_episode(&app, &ui);
            }
        });
    }

    // ---- add season dialog ----
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_open_add_season_dialog(move || {
            if let Some(ui) = ui_weak.upgrade() {
                open_add_season_dialog(&app, &ui);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        backend.on_close_add_season_dialog(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.global::<Backend>().set_add_season_dialog_open(false);
            }
        });
    }
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_submit_add_season(move || {
            if let Some(ui) = ui_weak.upgrade() {
                submit_add_season(&app, &ui);
            }
        });
    }

    // ---- settings dialog ----
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_open_settings_dialog(move || {
            if let Some(ui) = ui_weak.upgrade() {
                open_settings_dialog(&app, &ui);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        backend.on_close_settings_dialog(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.global::<Backend>().set_settings_dialog_open(false);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        backend.on_browse_settings_vlc(move || {
            if let Some(ui) = ui_weak.upgrade() {
                if let Some(path) = rfd::FileDialog::new().pick_file() {
                    ui.global::<Backend>().set_settings_vlc_path(path.display().to_string().into());
                }
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        backend.on_detect_settings_vlc(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let b = ui.global::<Backend>();
                match vlc::detect_vlc() {
                    Some(p) => {
                        b.set_settings_vlc_path(p.into());
                        b.set_status_text("Found VLC automatically.".into());
                    }
                    None => b.set_status_text("Could not find VLC automatically. Browse for it manually.".into()),
                }
            }
        });
    }
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_submit_settings(move || {
            if let Some(ui) = ui_weak.upgrade() {
                submit_settings(&app, &ui);
            }
        });
    }

    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_resume_last_played(move || {
            if let Some(ui) = ui_weak.upgrade() {
                resume_last_played(&app, &ui);
            }
        });
    }

    ui.run()?;
    Ok(())
}

// ---------------------------------------------------------------------
// Sidebar / detail rendering
// ---------------------------------------------------------------------

fn refresh_sidebar(app: &Arc<AppState>, ui: &MainWindow) {
    let conn = app.conn.lock().unwrap();
    let movies = db::list_entries(&conn, EntryKind::Movie).unwrap_or_default();
    let series = db::list_entries(&conn, EntryKind::Series).unwrap_or_default();
    drop(conn);

    let b = ui.global::<Backend>();
    b.set_movies(ModelRc::new(VecModel::from(
        movies
            .iter()
            .map(|e| EntryItem { id: e.id as i32, title: e.title.clone().into() })
            .collect::<Vec<_>>(),
    )));
    b.set_series(ModelRc::new(VecModel::from(
        series
            .iter()
            .map(|e| EntryItem { id: e.id as i32, title: e.title.clone().into() })
            .collect::<Vec<_>>(),
    )));
}

/// Groups a flat, season/episode-ordered episode list into per-season
/// timeline sections for the UI.
fn season_groups(episodes: &[Episode], selected_id: Option<i64>) -> ModelRc<SeasonGroup> {
    let mut groups: Vec<(i32, Vec<EpisodeItem>)> = Vec::new();
    for e in episodes {
        let item = EpisodeItem {
            id: e.id as i32,
            label: e.label().into(),
            selected: Some(e.id) == selected_id,
            finished: e.finished,
        };
        match groups.last_mut() {
            Some((season, items)) if *season == e.season => items.push(item),
            _ => groups.push((e.season, vec![item])),
        }
    }
    let season_structs: Vec<SeasonGroup> = groups
        .into_iter()
        .map(|(season, items)| SeasonGroup {
            season,
            label: format!("Season {season}").into(),
            episodes: ModelRc::new(VecModel::from(items)),
        })
        .collect();
    ModelRc::new(VecModel::from(season_structs))
}

fn select_entry(app: &Arc<AppState>, ui: &MainWindow, id: i64, _kind: &str) {
    let conn = app.conn.lock().unwrap();
    let entry = match db::get_entry(&conn, id) {
        Ok(e) => e,
        Err(_) => return,
    };
    let b = ui.global::<Backend>();
    b.set_selected_entry_id(id as i32);
    b.set_has_selection(true);
    b.set_detail_title(entry.title.clone().into());
    b.set_detail_kind(entry.kind.as_str().into());
    b.set_detail_description(entry.description.clone().into());
    b.set_status_text("".into());

    if entry.kind == EntryKind::Series {
        let episodes = db::list_episodes(&conn, id).unwrap_or_default();
        drop(conn);
        let first_id = episodes.first().map(|e| e.id);
        b.set_detail_seasons(season_groups(&episodes, first_id));
        let watched = !episodes.is_empty() && episodes.iter().all(|e| e.finished);
        b.set_detail_watched(watched);
        if let Some(first) = episodes.first() {
            b.set_detail_link(first.link_or_path.clone().into());
            b.set_detail_progress(progress_string(first.resume_position, first.duration).into());
            b.set_detail_episode_description(first.description.clone().into());
            b.set_resume_enabled(first.resume_position > 0);
            b.set_play_enabled(!first.link_or_path.is_empty());
        } else {
            b.set_detail_link("".into());
            b.set_detail_progress("".into());
            b.set_detail_episode_description("".into());
            b.set_resume_enabled(false);
            b.set_play_enabled(false);
        }
        b.set_top_resume_enabled(!episodes.is_empty());
        *app.selected.lock().unwrap() = Selection::Series { entry_id: id, episode_id: first_id };
    } else {
        b.set_detail_link(entry.link_or_path.clone().into());
        b.set_detail_seasons(ModelRc::new(VecModel::from(Vec::<SeasonGroup>::new())));
        b.set_detail_watched(entry.finished);
        b.set_detail_progress(progress_string(entry.resume_position, entry.duration).into());
        b.set_detail_episode_description("".into());
        b.set_resume_enabled(entry.resume_position > 0);
        b.set_play_enabled(!entry.link_or_path.is_empty());
        b.set_top_resume_enabled(!entry.link_or_path.is_empty());
        drop(conn);
        *app.selected.lock().unwrap() = Selection::Movie(id);
    }
}

fn select_episode(app: &Arc<AppState>, ui: &MainWindow, episode_id: i64) {
    let conn = app.conn.lock().unwrap();
    let ep = match db::get_episode(&conn, episode_id) {
        Ok(e) => e,
        Err(_) => return,
    };
    let entry_id = ep.entry_id;
    let episodes = db::list_episodes(&conn, entry_id).unwrap_or_default();
    drop(conn);

    let b = ui.global::<Backend>();
    b.set_detail_seasons(season_groups(&episodes, Some(episode_id)));
    b.set_detail_link(ep.link_or_path.clone().into());
    b.set_detail_progress(progress_string(ep.resume_position, ep.duration).into());
    b.set_detail_episode_description(ep.description.clone().into());
    b.set_resume_enabled(ep.resume_position > 0);
    b.set_play_enabled(!ep.link_or_path.is_empty());
    *app.selected.lock().unwrap() = Selection::Series { entry_id, episode_id: Some(episode_id) };
}

// ---------------------------------------------------------------------
// Playback
// ---------------------------------------------------------------------

fn do_play(app: &Arc<AppState>, ui: &MainWindow, resume: bool) {
    let sel = app.selected.lock().unwrap().clone();
    let conn = app.conn.lock().unwrap();
    let target_and_pos = match &sel {
        Selection::Movie(id) => db::get_entry(&conn, *id)
            .ok()
            .map(|e| (PlaybackTarget::Entry(*id, e.link_or_path), e.resume_position)),
        Selection::Series { episode_id: Some(eid), .. } => db::get_episode(&conn, *eid)
            .ok()
            .map(|e| (PlaybackTarget::Episode(*eid, e.link_or_path), e.resume_position)),
        _ => None,
    };

    if target_and_pos.is_some() {
        match &sel {
            Selection::Movie(id) => {
                let _ = db::set_last_played(&conn, EntryKind::Movie, *id, None);
            }
            Selection::Series { entry_id, episode_id } => {
                let _ = db::set_last_played(&conn, EntryKind::Series, *entry_id, *episode_id);
            }
            Selection::None => {}
        }
    }
    let vlc_path = resolve_vlc_path(&conn);
    drop(conn);

    let Some((target, resume_pos)) = target_and_pos else { return };
    if target.link().trim().is_empty() {
        ui.global::<Backend>().set_status_text("No link/path set for this item.".into());
        return;
    }
    let start = if resume { resume_pos } else { 0 };
    start_playback(app.clone(), ui.as_weak(), target, start, vlc_path);
}

/// Triggered by the "▶ Resume" button on the entry page header. Unlike the
/// per-episode Resume/Play buttons (which act on whatever's selected), this
/// jumps straight to the "current" thing to watch: the entry itself for a
/// movie, or the first not-yet-finished episode for a series (falling back
/// to the last episode once everything's been finished).
fn resume_entry(app: &Arc<AppState>, ui: &MainWindow) {
    let entry_id = match &*app.selected.lock().unwrap() {
        Selection::Movie(id) => *id,
        Selection::Series { entry_id, .. } => *entry_id,
        Selection::None => return,
    };
    let conn = app.conn.lock().unwrap();
    let entry = match db::get_entry(&conn, entry_id) {
        Ok(e) => e,
        Err(_) => return,
    };

    if entry.kind == EntryKind::Movie {
        drop(conn);
        select_entry(app, ui, entry_id, "movie");
        do_play(app, ui, true);
        return;
    }

    let current = db::current_episode(&conn, entry_id).ok().flatten();
    drop(conn);
    if let Some(ep) = current {
        select_entry(app, ui, entry_id, "series");
        select_episode(app, ui, ep.id);
        do_play(app, ui, true);
    }
}

/// Loads the last-played movie/series from settings and reflects it in the
/// "Resume: …" button shown on the empty landing page. For a series this
/// always resolves to the current "continue watching" episode rather than
/// trusting the exact episode last played, since that one might have been
/// finished (and auto-advanced) since.
fn refresh_last_played(app: &Arc<AppState>, ui: &MainWindow) {
    let conn = app.conn.lock().unwrap();
    let last = db::get_last_played(&conn).ok().flatten();
    let b = ui.global::<Backend>();
    match last {
        Some(lp) => {
            let label = match lp.kind {
                EntryKind::Movie => db::get_entry(&conn, lp.entry_id).map(|e| e.title).unwrap_or_default(),
                EntryKind::Series => {
                    let series_title = db::get_entry(&conn, lp.entry_id).map(|e| e.title).unwrap_or_default();
                    match db::current_episode(&conn, lp.entry_id).ok().flatten() {
                        Some(ep) => format!("{series_title} - {}", ep.label()),
                        None => series_title,
                    }
                }
            };
            b.set_has_last_played(true);
            b.set_last_played_label(label.into());
        }
        None => {
            b.set_has_last_played(false);
            b.set_last_played_label("".into());
        }
    }
}

/// Called when the user clicks the "Resume: …" button on the landing page:
/// navigate to that entry (and, for a series, its current episode) and
/// immediately resume playback.
fn resume_last_played(app: &Arc<AppState>, ui: &MainWindow) {
    let conn = app.conn.lock().unwrap();
    let last = db::get_last_played(&conn).ok().flatten();
    drop(conn);

    let Some(lp) = last else { return };
    match lp.kind {
        EntryKind::Movie => select_entry(app, ui, lp.entry_id, "movie"),
        EntryKind::Series => {
            select_entry(app, ui, lp.entry_id, "series");
            let conn = app.conn.lock().unwrap();
            let current = db::current_episode(&conn, lp.entry_id).ok().flatten();
            drop(conn);
            if let Some(ep) = current {
                select_episode(app, ui, ep.id);
            }
        }
    }
    do_play(app, ui, true);
}

fn start_playback(app: Arc<AppState>, ui_weak: slint::Weak<MainWindow>, target: PlaybackTarget, start_seconds: i64, vlc_path: String) {
    set_status_from_thread(&ui_weak, "Launching VLC...".to_string());
    thread::spawn(move || {
        let session = match vlc::launch(&vlc_path, target.link(), start_seconds) {
            Ok(s) => s,
            Err(e) => {
                set_status_from_thread(&ui_weak, format!("Failed to launch VLC: {e}"));
                return;
            }
        };
        let port = session.port;
        let password = session.password.clone();
        *app.vlc.lock().unwrap() = Some(session);
        set_status_from_thread(&ui_weak, "Playing in VLC...".to_string());

        let mut last_status = vlc::Status::default();
        loop {
            thread::sleep(Duration::from_millis(1500));
            let exited = {
                let mut guard = app.vlc.lock().unwrap();
                match guard.as_mut() {
                    Some(sess) => matches!(sess.child.try_wait(), Ok(Some(_)) | Err(_)),
                    None => true,
                }
            };
            if exited {
                break;
            }
            if let Ok(status) = vlc::query_status(port, &password) {
                last_status = status;
            }
        }

        let finished = last_status.length_seconds > 0
            && last_status.time_seconds as f64 >= last_status.length_seconds as f64 * FINISHED_THRESHOLD;
        let pos = if finished { 0 } else { last_status.time_seconds };
        {
            let conn = app.conn.lock().unwrap();
            let _ = target.persist(&conn, pos, last_status.length_seconds, finished);

            // Once an episode is finished, auto-advance the "current"
            // selection/last-played pointer to the next one in the series so
            // the timeline and both Resume buttons move forward on their own.
            if finished {
                if let PlaybackTarget::Episode(episode_id, _) = &target {
                    if let Ok(ep) = db::get_episode(&conn, *episode_id) {
                        if let Ok(Some(next)) = db::next_episode(&conn, ep.entry_id, ep.season, ep.episode) {
                            *app.selected.lock().unwrap() =
                                Selection::Series { entry_id: ep.entry_id, episode_id: Some(next.id) };
                            let _ = db::set_last_played(&conn, EntryKind::Series, ep.entry_id, Some(next.id));
                        }
                    }
                }
            }
        }
        *app.vlc.lock().unwrap() = None;

        let msg = if finished { "Finished. Moved to the next episode.".to_string() } else { "Stopped. Progress saved.".to_string() };
        set_status_from_thread(&ui_weak, msg);
        refresh_detail_from_thread(app.clone(), ui_weak.clone());
        refresh_last_played_from_thread(app, ui_weak);
    });
}

fn set_status_from_thread(ui_weak: &slint::Weak<MainWindow>, msg: String) {
    let ui_weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            ui.global::<Backend>().set_status_text(msg.into());
        }
    });
}

fn refresh_detail_from_thread(app: Arc<AppState>, ui_weak: slint::Weak<MainWindow>) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            let sel = app.selected.lock().unwrap().clone();
            match sel {
                Selection::Movie(id) => select_entry(&app, &ui, id, "movie"),
                Selection::Series { entry_id, episode_id: Some(eid) } => {
                    select_entry(&app, &ui, entry_id, "series");
                    select_episode(&app, &ui, eid);
                }
                Selection::Series { entry_id, episode_id: None } => select_entry(&app, &ui, entry_id, "series"),
                Selection::None => {}
            }
        }
    });
}

fn refresh_last_played_from_thread(app: Arc<AppState>, ui_weak: slint::Weak<MainWindow>) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            refresh_last_played(&app, &ui);
        }
    });
}

// ---------------------------------------------------------------------
// Add flow (new movie / new series)
// ---------------------------------------------------------------------

fn submit_add(app: &Arc<AppState>, ui: &MainWindow) {
    let b = ui.global::<Backend>();
    let title = b.get_form_title().to_string();
    let description = b.get_form_description().to_string();
    let kind_str = b.get_form_type().to_string();

    if title.trim().is_empty() {
        b.set_status_text("Title is required.".into());
        return;
    }

    if kind_str == "movie" {
        let link = b.get_form_link().to_string();
        if link.trim().is_empty() {
            b.set_status_text("Provide a file path or URL.".into());
            return;
        }
        let conn = app.conn.lock().unwrap();
        match db::insert_entry(&conn, &title, EntryKind::Movie, &description, &link) {
            Ok(_) => {
                drop(conn);
                refresh_sidebar(app, ui);
                b.set_add_dialog_open(false);
            }
            Err(e) => b.set_status_text(format!("Failed to save: {e}").into()),
        }
        return;
    }

    // Series: starting from the given season/episode numbers, keep
    // substituting increasing numbers into every "*"/"#" in the pattern and
    // verifying each generated link (network I/O) until one fails to
    // resolve (404) — that's the end of the range, discovered automatically
    // for both episodes within a season and seasons within the series.
    let pattern_str = b.get_pattern_link().to_string();
    let season_start: i32 = b.get_pattern_season_start().to_string().trim().parse().unwrap_or(1);
    let season_width: usize = b.get_pattern_season_width().to_string().trim().parse().unwrap_or(2);
    let ep_start: i32 = b.get_pattern_start().to_string().trim().parse().unwrap_or(1);
    let ep_width: usize = b.get_pattern_episode_width().to_string().trim().parse().unwrap_or(2);

    if pattern_str.trim().is_empty() || !pattern_str.contains('*') {
        b.set_status_text("Provide a link pattern containing '*' for the episode number.".into());
        return;
    }

    b.set_status_text("Probing for episodes... this may take a while.".into());
    let app2 = app.clone();
    let ui_weak = ui.as_weak();
    thread::spawn(move || {
        let progress_ui_weak = ui_weak.clone();
        let rows = match pattern::probe(&pattern_str, season_start, season_width, ep_start, ep_width, move |season, ep, ok| {
            if ok {
                set_status_from_thread(&progress_ui_weak, format!("Found S{season:02}E{ep:02}, checking next..."));
            }
        }) {
            Ok(v) => v,
            Err(e) => {
                set_status_from_thread(&ui_weak, format!("Error: {e}"));
                return;
            }
        };
        let total = rows.len();

        let conn = app2.conn.lock().unwrap();
        let entry_id = match db::insert_entry(&conn, &title, EntryKind::Series, &description, "") {
            Ok(id) => id,
            Err(e) => {
                drop(conn);
                set_status_from_thread(&ui_weak, format!("Failed to save series: {e}"));
                return;
            }
        };
        for (season, ep, link) in &rows {
            let ep_title = format!("Episode {ep}");
            let _ = db::insert_episode(&conn, entry_id, *season, *ep, &ep_title, "", link);
        }
        drop(conn);

        let msg = format!("Added series with {total} episode(s) found automatically.");
        let app3 = app2.clone();
        let ui_weak2 = ui_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak2.upgrade() {
                refresh_sidebar(&app3, &ui);
                let b = ui.global::<Backend>();
                b.set_add_dialog_open(false);
                b.set_status_text(msg.into());
                select_entry(&app3, &ui, entry_id, "series");
            }
        });
    });
}

// ---------------------------------------------------------------------
// Edit flow — entry title/description/link, or a single episode
// ---------------------------------------------------------------------

/// Always edits the whole entry (movie, or the series itself) currently
/// shown, regardless of whether an episode happens to be selected within it.
fn open_edit_entry_dialog(app: &Arc<AppState>, ui: &MainWindow) {
    let entry_id = match &*app.selected.lock().unwrap() {
        Selection::Movie(id) => *id,
        Selection::Series { entry_id, .. } => *entry_id,
        Selection::None => return,
    };
    let conn = app.conn.lock().unwrap();
    let entry = match db::get_entry(&conn, entry_id) {
        Ok(e) => e,
        Err(_) => return,
    };
    drop(conn);

    *app.pending_edit.lock().unwrap() = Some(EditTarget::Entry(entry_id));
    let b = ui.global::<Backend>();
    b.set_edit_title(entry.title.into());
    b.set_edit_description(entry.description.into());
    b.set_edit_link(entry.link_or_path.into());
    b.set_edit_is_episode(false);
    b.set_edit_dialog_open(true);
}

fn open_edit_episode_dialog(app: &Arc<AppState>, ui: &MainWindow, episode_id: i64) {
    let conn = app.conn.lock().unwrap();
    let ep = match db::get_episode(&conn, episode_id) {
        Ok(e) => e,
        Err(_) => return,
    };
    drop(conn);

    *app.pending_edit.lock().unwrap() = Some(EditTarget::Episode(episode_id));
    let b = ui.global::<Backend>();
    b.set_edit_title(ep.title.into());
    b.set_edit_description(ep.description.into());
    b.set_edit_link(ep.link_or_path.into());
    b.set_edit_is_episode(true);
    b.set_edit_dialog_open(true);
}

fn submit_edit(app: &Arc<AppState>, ui: &MainWindow) {
    let b = ui.global::<Backend>();
    let title = b.get_edit_title().to_string();
    let description = b.get_edit_description().to_string();
    let link = b.get_edit_link().to_string();

    let target = app.pending_edit.lock().unwrap().clone();
    let conn = app.conn.lock().unwrap();
    let result = match &target {
        Some(EditTarget::Entry(id)) => db::update_entry(&conn, *id, &title, &description, &link),
        Some(EditTarget::Episode(id)) => db::update_episode(&conn, *id, &title, &description, &link),
        None => Ok(()),
    };
    drop(conn);

    match result {
        Ok(()) => {
            b.set_edit_dialog_open(false);
            match target {
                Some(EditTarget::Entry(id)) => {
                    let conn = app.conn.lock().unwrap();
                    let kind = db::get_entry(&conn, id).map(|e| e.kind).unwrap_or(EntryKind::Movie);
                    drop(conn);
                    refresh_sidebar(app, ui);
                    select_entry(app, ui, id, kind.as_str());
                    refresh_last_played(app, ui);
                }
                Some(EditTarget::Episode(id)) => {
                    let conn = app.conn.lock().unwrap();
                    let entry_id = db::get_episode(&conn, id).ok().map(|e| e.entry_id);
                    drop(conn);
                    if let Some(entry_id) = entry_id {
                        select_entry(app, ui, entry_id, "series");
                    }
                    refresh_last_played(app, ui);
                }
                None => {}
            }
        }
        Err(e) => b.set_status_text(format!("Failed to save: {e}").into()),
    }
}

// ---------------------------------------------------------------------
// Delete flow — entry, episode, or whole season
// ---------------------------------------------------------------------

/// Triggered by the "Delete" button on the entry detail header. Always
/// targets the whole entry (movie, or the entire series) currently shown —
/// even if an individual episode happens to be selected within it.
fn request_delete_entry(app: &Arc<AppState>, ui: &MainWindow) {
    let entry_id = match &*app.selected.lock().unwrap() {
        Selection::Movie(id) => *id,
        Selection::Series { entry_id, .. } => *entry_id,
        Selection::None => return,
    };
    let conn = app.conn.lock().unwrap();
    let title = db::get_entry(&conn, entry_id).map(|e| e.title).unwrap_or_default();
    drop(conn);

    *app.pending_delete.lock().unwrap() = Some(DeleteTarget::Entry(entry_id));
    let b = ui.global::<Backend>();
    b.set_confirm_message(format!("Delete \"{title}\"? This cannot be undone.").into());
    b.set_confirm_dialog_open(true);
}

/// Triggered by the "✎"/"✕" controls on an individual timeline episode.
fn request_delete_episode(app: &Arc<AppState>, ui: &MainWindow, episode_id: i64) {
    let conn = app.conn.lock().unwrap();
    let label = match db::get_episode(&conn, episode_id) {
        Ok(e) => e.label(),
        Err(_) => return,
    };
    drop(conn);

    *app.pending_delete.lock().unwrap() = Some(DeleteTarget::Episode(episode_id));
    let b = ui.global::<Backend>();
    b.set_confirm_message(format!("Delete episode \"{label}\"?").into());
    b.set_confirm_dialog_open(true);
}

/// Triggered by "Delete season" on a season header.
fn request_delete_season(app: &Arc<AppState>, ui: &MainWindow, season: i32) {
    let entry_id = match &*app.selected.lock().unwrap() {
        Selection::Series { entry_id, .. } => *entry_id,
        _ => return,
    };
    *app.pending_delete.lock().unwrap() = Some(DeleteTarget::Season(entry_id, season));
    let b = ui.global::<Backend>();
    b.set_confirm_message(format!("Delete Season {season} and all its episodes? This cannot be undone.").into());
    b.set_confirm_dialog_open(true);
}

fn confirm_delete(app: &Arc<AppState>, ui: &MainWindow) {
    let target = app.pending_delete.lock().unwrap().take();
    let b = ui.global::<Backend>();
    b.set_confirm_dialog_open(false);

    match target {
        Some(DeleteTarget::Entry(id)) => {
            let conn = app.conn.lock().unwrap();
            if let Ok(Some(lp)) = db::get_last_played(&conn) {
                if lp.entry_id == id {
                    let _ = db::clear_last_played(&conn);
                }
            }
            let result = db::delete_entry(&conn, id);
            drop(conn);

            match result {
                Ok(()) => {
                    *app.selected.lock().unwrap() = Selection::None;
                    b.set_has_selection(false);
                    b.set_detail_title("".into());
                    b.set_detail_description("".into());
                    b.set_detail_link("".into());
                    b.set_detail_progress("".into());
                    b.set_detail_episode_description("".into());
                    b.set_detail_watched(false);
                    b.set_detail_seasons(ModelRc::new(VecModel::from(Vec::<SeasonGroup>::new())));
                    b.set_status_text("".into());
                    refresh_sidebar(app, ui);
                    refresh_last_played(app, ui);
                }
                Err(e) => b.set_status_text(format!("Failed to delete: {e}").into()),
            }
        }
        Some(DeleteTarget::Episode(id)) => {
            let conn = app.conn.lock().unwrap();
            let entry_id = db::get_episode(&conn, id).ok().map(|e| e.entry_id);
            if let Ok(Some(lp)) = db::get_last_played(&conn) {
                if lp.episode_id == Some(id) {
                    let _ = db::clear_last_played(&conn);
                }
            }
            let result = db::delete_episode(&conn, id);
            drop(conn);

            match result {
                Ok(()) => {
                    refresh_last_played(app, ui);
                    if let Some(entry_id) = entry_id {
                        select_entry(app, ui, entry_id, "series");
                    }
                }
                Err(e) => b.set_status_text(format!("Failed to delete: {e}").into()),
            }
        }
        Some(DeleteTarget::Season(entry_id, season)) => {
            let conn = app.conn.lock().unwrap();
            if let Ok(Some(lp)) = db::get_last_played(&conn) {
                if let Some(eid) = lp.episode_id {
                    if let Ok(ep) = db::get_episode(&conn, eid) {
                        if ep.entry_id == entry_id && ep.season == season {
                            let _ = db::clear_last_played(&conn);
                        }
                    }
                }
            }
            let result = db::delete_season(&conn, entry_id, season);
            drop(conn);

            match result {
                Ok(()) => {
                    refresh_last_played(app, ui);
                    select_entry(app, ui, entry_id, "series");
                }
                Err(e) => b.set_status_text(format!("Failed to delete season: {e}").into()),
            }
        }
        None => {}
    }
}

// ---------------------------------------------------------------------
// Add episode (single, manual) / add season (auto-detected range)
// ---------------------------------------------------------------------

fn open_add_episode_dialog(app: &Arc<AppState>, ui: &MainWindow, season: i32) {
    let entry_id = match &*app.selected.lock().unwrap() {
        Selection::Series { entry_id, .. } => *entry_id,
        _ => return,
    };
    let conn = app.conn.lock().unwrap();
    let episodes = db::list_episodes(&conn, entry_id).unwrap_or_default();
    drop(conn);
    let next_ep = episodes
        .iter()
        .filter(|e| e.season == season)
        .map(|e| e.episode)
        .max()
        .map(|m| m + 1)
        .unwrap_or(1);

    let b = ui.global::<Backend>();
    b.set_add_episode_season(season.to_string().into());
    b.set_add_episode_number(next_ep.to_string().into());
    b.set_add_episode_title("".into());
    b.set_add_episode_description("".into());
    b.set_add_episode_link("".into());
    b.set_status_text("".into());
    b.set_add_episode_dialog_open(true);
}

fn submit_add_episode(app: &Arc<AppState>, ui: &MainWindow) {
    let entry_id = match &*app.selected.lock().unwrap() {
        Selection::Series { entry_id, .. } => *entry_id,
        _ => return,
    };
    let b = ui.global::<Backend>();
    let season: i32 = b.get_add_episode_season().to_string().trim().parse().unwrap_or(1);
    let episode: i32 = b.get_add_episode_number().to_string().trim().parse().unwrap_or(1);
    let title = b.get_add_episode_title().to_string();
    let description = b.get_add_episode_description().to_string();
    let link = b.get_add_episode_link().to_string();

    if link.trim().is_empty() {
        b.set_status_text("Provide a file path or URL for the episode.".into());
        return;
    }
    let ep_title = if title.trim().is_empty() { format!("Episode {episode}") } else { title };

    let conn = app.conn.lock().unwrap();
    let result = db::insert_episode(&conn, entry_id, season, episode, &ep_title, &description, &link);
    drop(conn);

    match result {
        Ok(new_id) => {
            b.set_add_episode_dialog_open(false);
            select_entry(app, ui, entry_id, "series");
            select_episode(app, ui, new_id);
        }
        Err(e) => b.set_status_text(format!("Failed to add episode: {e}").into()),
    }
}

fn open_add_season_dialog(app: &Arc<AppState>, ui: &MainWindow) {
    let entry_id = match &*app.selected.lock().unwrap() {
        Selection::Series { entry_id, .. } => *entry_id,
        _ => return,
    };
    let conn = app.conn.lock().unwrap();
    let episodes = db::list_episodes(&conn, entry_id).unwrap_or_default();
    drop(conn);
    let next_season = episodes.iter().map(|e| e.season).max().map(|m| m + 1).unwrap_or(1);

    let b = ui.global::<Backend>();
    b.set_add_season_number(next_season.to_string().into());
    b.set_add_season_width("2".into());
    b.set_add_season_pattern("".into());
    b.set_add_season_ep_start("1".into());
    b.set_add_season_ep_width("2".into());
    b.set_status_text("".into());
    b.set_add_season_dialog_open(true);
}

fn submit_add_season(app: &Arc<AppState>, ui: &MainWindow) {
    let entry_id = match &*app.selected.lock().unwrap() {
        Selection::Series { entry_id, .. } => *entry_id,
        _ => return,
    };
    let b = ui.global::<Backend>();
    let season: i32 = b.get_add_season_number().to_string().trim().parse().unwrap_or(1);
    let season_width: usize = b.get_add_season_width().to_string().trim().parse().unwrap_or(2);
    let pattern_str = b.get_add_season_pattern().to_string();
    let ep_start: i32 = b.get_add_season_ep_start().to_string().trim().parse().unwrap_or(1);
    let ep_width: usize = b.get_add_season_ep_width().to_string().trim().parse().unwrap_or(2);

    if pattern_str.trim().is_empty() || !pattern_str.contains('*') {
        b.set_status_text("Provide a link pattern containing '*' for the episode number.".into());
        return;
    }

    b.set_status_text("Probing for episodes... this may take a while.".into());
    let app2 = app.clone();
    let ui_weak = ui.as_weak();
    thread::spawn(move || {
        let progress_ui_weak = ui_weak.clone();
        let rows = match pattern::probe_season(&pattern_str, season, season_width, ep_start, ep_width, move |s, ep, ok| {
            if ok {
                set_status_from_thread(&progress_ui_weak, format!("Found S{s:02}E{ep:02}, checking next..."));
            }
        }) {
            Ok(v) => v,
            Err(e) => {
                set_status_from_thread(&ui_weak, format!("Error: {e}"));
                return;
            }
        };
        let total = rows.len();

        let conn = app2.conn.lock().unwrap();
        for (s, ep, link) in &rows {
            let ep_title = format!("Episode {ep}");
            let _ = db::insert_episode(&conn, entry_id, *s, *ep, &ep_title, "", link);
        }
        drop(conn);

        let msg = format!("Added season {season} with {total} episode(s).");
        let app3 = app2.clone();
        let ui_weak2 = ui_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak2.upgrade() {
                let b = ui.global::<Backend>();
                b.set_add_season_dialog_open(false);
                b.set_status_text(msg.into());
                select_entry(&app3, &ui, entry_id, "series");
            }
        });
    });
}

// ---------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------

fn open_settings_dialog(app: &Arc<AppState>, ui: &MainWindow) {
    let conn = app.conn.lock().unwrap();
    let path = db::get_vlc_path(&conn).ok().flatten().unwrap_or_default();
    drop(conn);

    let b = ui.global::<Backend>();
    b.set_settings_vlc_path(path.into());
    b.set_status_text("".into());
    b.set_settings_dialog_open(true);
}

fn submit_settings(app: &Arc<AppState>, ui: &MainWindow) {
    let b = ui.global::<Backend>();
    let path = b.get_settings_vlc_path().to_string();

    let conn = app.conn.lock().unwrap();
    let result = db::set_vlc_path(&conn, &path);
    drop(conn);

    match result {
        Ok(()) => {
            b.set_settings_dialog_open(false);
            b.set_status_text("Settings saved.".into());
        }
        Err(e) => b.set_status_text(format!("Failed to save settings: {e}").into()),
    }
}
