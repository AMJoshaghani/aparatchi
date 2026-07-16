// Debug builds keep the console around since it's handy for println!
// debugging in Windows. No effect on Linux/macOS
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod db;
mod models;
mod pattern;
mod vlc;

use models::{EntryKind, Episode};
use rusqlite::Connection;
use slint::{ComponentHandle, ModelRc, VecModel};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;


slint::include_modules!();

// We call a movie/episode "finished" once it's been played through this
// much of its runtime
const FINISHED_THRESHOLD: f64 = 0.90;

// Figures out which VLC to actually run (set in settings)
fn resolve_vlc_path(conn: &Connection) -> String {
    if let Ok(Some(p)) = db::get_vlc_path(conn) {
        if !p.trim().is_empty() {
            return p;
        }
    }
    vlc::detect_vlc().unwrap_or_else(|| vlc::default_binary_name().to_string())
}

// Whatever subtitle language the user asked for in Settings ("eng", "fas"),
// or an empty string if they never set one, in which case vlc uses default
fn resolve_subtitle_lang(conn: &Connection) -> String {
    db::get_subtitle_lang(conn).ok().flatten().unwrap_or_default()
}

// ---------------------------------------------------------------------
// Posters / thumbnails
//
// We never store poster paths in the database. Every entry's poster (if it
// has one) just lives at a filename built from its id
// ---------------------------------------------------------------------

const PLACEHOLDER_POSTER_BYTES: &[u8] = include_bytes!("../ui/poster_placeholder.png");

fn posters_dir() -> PathBuf {
    db::data_dir().join("posters")
}

// Drops the placeholder poster onto disk if it's not already there,
// Only needs to run once, at startup.
fn ensure_placeholder_poster() {
    let dir = posters_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("placeholder.png");
    if !path.exists() {
        let _ = std::fs::write(&path, PLACEHOLDER_POSTER_BYTES);
    }
}

fn poster_file(entry_id: i64, thumb: bool) -> PathBuf {
    let name = if thumb { format!("{entry_id}_thumb.png") } else { format!("{entry_id}.png") };
    posters_dir().join(name)
}

// Grabs the poster or thumbnail for an entry
fn load_poster_image(entry_id: i64, thumb: bool) -> slint::Image {
    let custom = poster_file(entry_id, thumb);
    let path = if custom.exists() { custom } else { posters_dir().join("placeholder.png") };
    slint::Image::load_from_path(&path).unwrap_or_default()
}

// Takes whatever image the user picked and generates both sizes we
// actually need from it: a full poster and a small thumbnail for
// the sidebar.
fn save_poster(entry_id: i64, source_path: &str) -> anyhow::Result<()> {
    let img = image::open(Path::new(source_path))?;
    let dir = posters_dir();
    std::fs::create_dir_all(&dir)?;

    let full = img.resize(800, 1200, image::imageops::FilterType::Lanczos3);
    full.save(poster_file(entry_id, false))?;

    let thumb = img.resize(90, 135, image::imageops::FilterType::Lanczos3);
    thumb.save(poster_file(entry_id, true))?;

    Ok(())
}

fn delete_poster_files(entry_id: i64) {
    let _ = std::fs::remove_file(poster_file(entry_id, false));
    let _ = std::fs::remove_file(poster_file(entry_id, true));
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

// Builds the  progress readout, or an empty string if we don't even know
// the runtime yet.
fn progress_string(resume_position: i64, duration: i64, finished: bool) -> String {
    if duration > 0 {
        let shown_pos = if finished { duration } else { resume_position };
        format!("{} / {}", format_duration(shown_pos), format_duration(duration))
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
    BulkEpisodes(Vec<i64>),
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
    /// Which episode ids are ticked while the timeline's in bulk-select mode.
    bulk_selected: Mutex<HashSet<i64>>,
}

fn main() -> anyhow::Result<()> {
    let conn = db::open()?;
    ensure_placeholder_poster();
    let app = Arc::new(AppState {
        conn: Mutex::new(conn),
        selected: Mutex::new(Selection::None),
        vlc: Mutex::new(None),
        pending_delete: Mutex::new(None),
        pending_edit: Mutex::new(None),
        bulk_selected: Mutex::new(HashSet::new()),
    });

    let ui = MainWindow::new()?;
    refresh_sidebar(&app, &ui);
    refresh_last_played(&app, &ui);

    // If nobody's told us where VLC lives yet take a guess now so playback
    // just works without making anyone visit Settings first.
    {
        let conn = app.conn.lock().unwrap();
        if db::get_vlc_path(&conn).ok().flatten().is_none() {
            if let Some(detected) = vlc::detect_vlc() {
                let _ = db::set_vlc_path(&conn, &detected);
            }
        }
    }

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
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_select_next_episode(move || {
            if let Some(ui) = ui_weak.upgrade() {
                select_adjacent_episode(&app, &ui, 1);
            }
        });
    }
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_select_previous_episode(move || {
            if let Some(ui) = ui_weak.upgrade() {
                select_adjacent_episode(&app, &ui, -1);
            }
        });
    }
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_request_delete_current(move || {
            if let Some(ui) = ui_weak.upgrade() {
                request_delete_current(&app, &ui);
            }
        });
    }
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_jump_to_next_unfinished(move || {
            if let Some(ui) = ui_weak.upgrade() {
                jump_to_next_unfinished(&app, &ui);
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
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_toggle_movie_watched(move || {
            if let Some(ui) = ui_weak.upgrade() {
                toggle_movie_watched(&app, &ui);
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
                b.set_form_poster_path("".into());
                b.set_form_type("movie".into());
                b.set_pattern_link("".into());
                b.set_pattern_season_start("1".into());
                b.set_pattern_season_width("2".into());
                b.set_pattern_start("1".into());
                b.set_pattern_episode_width("2".into());
                b.set_add_in_progress(false);
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
        let ui_weak = ui.as_weak();
        backend.on_browse_form_poster(move || {
            if let Some(ui) = ui_weak.upgrade() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Images", &["png", "jpg", "jpeg", "gif", "webp", "bmp"])
                    .pick_file()
                {
                    ui.global::<Backend>().set_form_poster_path(path.display().to_string().into());
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
        let ui_weak = ui.as_weak();
        backend.on_browse_edit_poster(move || {
            if let Some(ui) = ui_weak.upgrade() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Images", &["png", "jpg", "jpeg", "gif", "webp", "bmp"])
                    .pick_file()
                {
                    ui.global::<Backend>().set_edit_poster_path(path.display().to_string().into());
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
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_request_reprobe_season(move |season| {
            if let Some(ui) = ui_weak.upgrade() {
                request_reprobe_season(&app, &ui, season);
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

    // ---- bulk episode select/actions ----
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_toggle_bulk_select_mode(move || {
            if let Some(ui) = ui_weak.upgrade() {
                toggle_bulk_select_mode(&app, &ui);
            }
        });
    }
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_toggle_bulk_selected(move |id| {
            if let Some(ui) = ui_weak.upgrade() {
                toggle_bulk_selected(&app, &ui, id as i64);
            }
        });
    }
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_bulk_delete_selected(move || {
            if let Some(ui) = ui_weak.upgrade() {
                bulk_delete_selected(&app, &ui);
            }
        });
    }
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_bulk_mark_watched(move || {
            if let Some(ui) = ui_weak.upgrade() {
                bulk_set_finished(&app, &ui, true);
            }
        });
    }
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_bulk_mark_unwatched(move || {
            if let Some(ui) = ui_weak.upgrade() {
                bulk_set_finished(&app, &ui, false);
            }
        });
    }

    // ---- search / sort ----
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_refresh_lists(move || {
            if let Some(ui) = ui_weak.upgrade() {
                refresh_sidebar(&app, &ui);
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

    // ---- data management ----
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_export_library(move || {
            if let Some(ui) = ui_weak.upgrade() {
                export_library(&app, &ui);
            }
        });
    }
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_import_library(move || {
            if let Some(ui) = ui_weak.upgrade() {
                import_library(&app, &ui);
            }
        });
    }
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        backend.on_check_all_links(move || {
            if let Some(ui) = ui_weak.upgrade() {
                check_all_links(&app, &ui);
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

fn sort_entries(entries: &mut [models::Entry], mode: &str) {
    match mode {
        "Recently added" => entries.sort_by(|a, b| b.id.cmp(&a.id)),
        "Recently watched" => entries.sort_by(|a, b| b.last_watched_at.cmp(&a.last_watched_at)),
        _ => entries.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase())), // "Title (A-Z)"
    }
}

fn refresh_sidebar(app: &Arc<AppState>, ui: &MainWindow) {
    let b = ui.global::<Backend>();
    let query = b.get_search_query().to_string().to_lowercase();
    let sort_mode = b.get_sort_mode().to_string();

    let conn = app.conn.lock().unwrap();
    let mut movies = db::list_entries(&conn, EntryKind::Movie).unwrap_or_default();
    let mut series = db::list_entries(&conn, EntryKind::Series).unwrap_or_default();
    drop(conn);

    if !query.trim().is_empty() {
        movies.retain(|e| e.title.to_lowercase().contains(query.trim()));
        series.retain(|e| e.title.to_lowercase().contains(query.trim()));
    }
    sort_entries(&mut movies, &sort_mode);
    sort_entries(&mut series, &sort_mode);

    b.set_movies(ModelRc::new(VecModel::from(
        movies
            .iter()
            .map(|e| EntryItem { id: e.id as i32, title: e.title.clone().into(), poster: load_poster_image(e.id, true) })
            .collect::<Vec<_>>(),
    )));
    b.set_series(ModelRc::new(VecModel::from(
        series
            .iter()
            .map(|e| EntryItem { id: e.id as i32, title: e.title.clone().into(), poster: load_poster_image(e.id, true) })
            .collect::<Vec<_>>(),
    )));
}

// Takes the flat, already-ordered episode list and buckets it up by season
// for the timeline UI. `checked` is whatever's currently ticked in
// bulk-select mode - it's just an empty set when that mode's off.
fn season_groups(episodes: &[Episode], selected_id: Option<i64>, checked: &HashSet<i64>) -> ModelRc<SeasonGroup> {
    let mut groups: Vec<(i32, Vec<EpisodeItem>)> = Vec::new();
    for e in episodes {
        let item = EpisodeItem {
            id: e.id as i32,
            label: e.label().into(),
            selected: Some(e.id) == selected_id,
            finished: e.finished,
            checked: checked.contains(&e.id),
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

// Redraws just the season/episode timeline for whatever series is
// currently open, picking up any changes to finished/checked state -
// without touching the rest of the detail pane.
fn refresh_current_series_timeline(app: &Arc<AppState>, ui: &MainWindow) {
    let sel = app.selected.lock().unwrap().clone();
    if let Selection::Series { entry_id, episode_id } = sel {
        let conn = app.conn.lock().unwrap();
        let episodes = db::list_episodes(&conn, entry_id).unwrap_or_default();
        drop(conn);
        let checked = app.bulk_selected.lock().unwrap();
        ui.global::<Backend>().set_detail_seasons(season_groups(&episodes, episode_id, &checked));
    }
}

fn select_entry(app: &Arc<AppState>, ui: &MainWindow, id: i64, _kind: &str) {
    let switching_entry = match &*app.selected.lock().unwrap() {
        Selection::Movie(cur) => *cur != id,
        Selection::Series { entry_id, .. } => *entry_id != id,
        Selection::None => true,
    };
    if switching_entry {
        app.bulk_selected.lock().unwrap().clear();
        ui.global::<Backend>().set_bulk_select_mode(false);
    }

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
    b.set_detail_poster(load_poster_image(id, false));
    b.set_status_text("".into());

    if entry.kind == EntryKind::Series {
        let episodes = db::list_episodes(&conn, id).unwrap_or_default();
        drop(conn);
        let first_id = episodes.first().map(|e| e.id);
        let checked = app.bulk_selected.lock().unwrap();
        b.set_detail_seasons(season_groups(&episodes, first_id, &checked));
        drop(checked);
        let watched = !episodes.is_empty() && episodes.iter().all(|e| e.finished);
        b.set_detail_watched(watched);
        if let Some(first) = episodes.first() {
            b.set_detail_link(first.link_or_path.clone().into());
            b.set_detail_progress(progress_string(first.resume_position, first.duration, first.finished).into());
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
        b.set_detail_progress(progress_string(entry.resume_position, entry.duration, entry.finished).into());
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
    let checked = app.bulk_selected.lock().unwrap();
    b.set_detail_seasons(season_groups(&episodes, Some(episode_id), &checked));
    drop(checked);
    b.set_detail_link(ep.link_or_path.clone().into());
    b.set_detail_progress(progress_string(ep.resume_position, ep.duration, ep.finished).into());
    b.set_detail_episode_description(ep.description.clone().into());
    b.set_resume_enabled(ep.resume_position > 0);
    b.set_play_enabled(!ep.link_or_path.is_empty());
    *app.selected.lock().unwrap() = Selection::Series { entry_id, episode_id: Some(episode_id) };
}

// Steps the selected episode forward or back (direction +1/-1) through the
// full, season-then-episode-ordered list. This is what the up/down arrow
// shortcuts call. Does nothing if you're already at either end.
fn select_adjacent_episode(app: &Arc<AppState>, ui: &MainWindow, direction: i32) {
    let (entry_id, current_id) = match &*app.selected.lock().unwrap() {
        Selection::Series { entry_id, episode_id } => (*entry_id, *episode_id),
        _ => return,
    };
    let conn = app.conn.lock().unwrap();
    let episodes = db::list_episodes(&conn, entry_id).unwrap_or_default();
    drop(conn);
    if episodes.is_empty() {
        return;
    }
    let next_index = match current_id.and_then(|cid| episodes.iter().position(|e| e.id == cid)) {
        Some(i) => {
            let new_i = i as i32 + direction;
            if new_i < 0 || new_i as usize >= episodes.len() {
                return;
            }
            new_i as usize
        }
        None => 0,
    };
    select_episode(app, ui, episodes[next_index].id);
}

// Jumps the selection to the first not-yet-finished episode in the current
// series - the same one the "Resume" button would take you to - but
// without actually starting playback. Handy for just browsing to where
// you left off.
fn jump_to_next_unfinished(app: &Arc<AppState>, ui: &MainWindow) {
    let entry_id = match &*app.selected.lock().unwrap() {
        Selection::Series { entry_id, .. } => *entry_id,
        _ => return,
    };
    let conn = app.conn.lock().unwrap();
    let ep = db::first_unfinished_episode(&conn, entry_id).ok().flatten();
    drop(conn);
    match ep {
        Some(ep) => select_episode(app, ui, ep.id),
        None => ui.global::<Backend>().set_status_text("All episodes are finished.".into()),
    }
}

// What the Delete key actually does: remove whichever episode is
// currently highlighted in a series, or the whole entry if it's a movie
// (or a series with nothing specific highlighted).
fn request_delete_current(app: &Arc<AppState>, ui: &MainWindow) {
    let sel = app.selected.lock().unwrap().clone();
    match sel {
        Selection::Movie(_) => request_delete_entry(app, ui),
        Selection::Series { episode_id: Some(eid), .. } => request_delete_episode(app, ui, eid),
        Selection::Series { episode_id: None, .. } => request_delete_entry(app, ui),
        Selection::None => {}
    }
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
                let _ = db::touch_last_watched(&conn, *id);
            }
            Selection::Series { entry_id, episode_id } => {
                let _ = db::set_last_played(&conn, EntryKind::Series, *entry_id, *episode_id);
                let _ = db::touch_last_watched(&conn, *entry_id);
            }
            Selection::None => {}
        }
    }
    let vlc_path = resolve_vlc_path(&conn);
    let subtitle_lang = resolve_subtitle_lang(&conn);
    drop(conn);

    let Some((target, resume_pos)) = target_and_pos else { return };
    if target.link().trim().is_empty() {
        ui.global::<Backend>().set_status_text("No link/path set for this item.".into());
        return;
    }
    let start = if resume { resume_pos } else { 0 };
    start_playback(app.clone(), ui.as_weak(), target, start, vlc_path, subtitle_lang);
}

// This is what the "Resume" button up in the entry header does. It's
// different from the per-episode Resume/Play buttons, which just act on
// whatever happens to be selected - this one jumps straight to the
// "current" thing to watch: the movie itself, or for a series, the first
// episode that isn't finished yet (or the last one, if you've somehow
// finished them all).
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

// Pulls up whatever was last played and updates the "Resume: ..." button
// on the empty landing page to match. For a series we don't just trust the
// exact episode that was last played - it might be finished and
// auto-advanced past by now - so we always resolve to whatever the
// "continue watching" episode currently is.
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

    // Build the "Continue watching" list: anything with progress that isn't
    // finished yet, most recently watched first, trimmed down to a short
    // list so the landing page doesn't turn into a wall of entries.
    let mut in_progress: Vec<(i64, &'static str, String, i64)> = Vec::new();
    for m in db::list_entries(&conn, EntryKind::Movie).unwrap_or_default() {
        if m.resume_position > 0 && !m.finished {
            in_progress.push((m.id, "movie", m.title, m.last_watched_at));
        }
    }
    for s in db::list_entries(&conn, EntryKind::Series).unwrap_or_default() {
        let episodes = db::list_episodes(&conn, s.id).unwrap_or_default();
        let has_progress = episodes.iter().any(|e| e.resume_position > 0 && !e.finished);
        if has_progress {
            in_progress.push((s.id, "series", s.title, s.last_watched_at));
        }
    }
    drop(conn);

    in_progress.sort_by(|a, b| b.3.cmp(&a.3));
    in_progress.truncate(5);
    let items: Vec<ContinueItem> = in_progress
        .into_iter()
        .map(|(id, kind, title, _)| ContinueItem { id: id as i32, kind: kind.into(), title: title.into() })
        .collect();
    b.set_continue_watching(ModelRc::new(VecModel::from(items)));
}

// Fires when someone clicks "Resume: ..." on the landing page: jump to
// that entry (and for a series, whatever its current episode is) and
// start playing right away.
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

fn start_playback(
    app: Arc<AppState>,
    ui_weak: slint::Weak<MainWindow>,
    target: PlaybackTarget,
    start_seconds: i64,
    vlc_path: String,
    subtitle_lang: String,
) {
    set_status_from_thread(&ui_weak, "Launching VLC...".to_string());
    thread::spawn(move || {
        let session = match vlc::launch(&vlc_path, target.link(), start_seconds, &subtitle_lang) {
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
                // Once a video plays all the way through, VLC doesn't close -
                // it just goes idle and starts reporting length=0 (nothing's
                // loaded anymore). If we kept overwriting last_status with
                // that, a fully-watched episode would look "unplayed" by the
                // time the user actually closes VLC. So we only accept a new
                // reading while something's actually loaded, and just hang
                // onto the last real one otherwise.
                if status.length_seconds > 0 {
                    last_status = status;
                }
            }
        }

        let finished = last_status.length_seconds > 0
            && last_status.time_seconds as f64 >= last_status.length_seconds as f64 * FINISHED_THRESHOLD;
        let pos = if finished { 0 } else { last_status.time_seconds };
        {
            let conn = app.conn.lock().unwrap();
            let _ = target.persist(&conn, pos, last_status.length_seconds, finished);

            // Once an episode wraps up, quietly move the "current" pointer
            // to the next one in the series, so the timeline and both Resume
            // buttons keep pace without anyone having to click ahead manually.
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

fn set_add_in_progress_from_thread(ui_weak: &slint::Weak<MainWindow>, value: bool) {
    let ui_weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            ui.global::<Backend>().set_add_in_progress(value);
        }
    });
}

fn set_add_season_in_progress_from_thread(ui_weak: &slint::Weak<MainWindow>, value: bool) {
    let ui_weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            ui.global::<Backend>().set_add_season_in_progress(value);
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
        let poster_source = b.get_form_poster_path().to_string();
        let conn = app.conn.lock().unwrap();
        match db::insert_entry(&conn, &title, EntryKind::Movie, &description, &link) {
            Ok(new_id) => {
                drop(conn);
                if !poster_source.trim().is_empty() {
                    let _ = save_poster(new_id, &poster_source);
                }
                refresh_sidebar(app, ui);
                b.set_add_dialog_open(false);
            }
            Err(e) => b.set_status_text(format!("Failed to save: {e}").into()),
        }
        return;
    }

    // For a series, we start at the given season/episode numbers and keep
    // plugging bigger numbers into every "*"/"#" in the pattern, checking
    // each generated link over the network as we go. The moment one fails
    // to resolve (a 404, basically), we know we've hit the end - and this
    // works the same way for finding the last episode in a season and the
    // last season in the series.
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
    b.set_add_in_progress(true);
    let poster_source = b.get_form_poster_path().to_string();
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
                set_add_in_progress_from_thread(&ui_weak, false);
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
                set_add_in_progress_from_thread(&ui_weak, false);
                return;
            }
        };
        for (season, ep, link) in &rows {
            let ep_title = format!("Episode {ep}");
            let _ = db::insert_episode(&conn, entry_id, *season, *ep, &ep_title, "", link);
        }
        let mut seasons_seen: Vec<i32> = rows.iter().map(|(s, _, _)| *s).collect();
        seasons_seen.dedup();
        for s in seasons_seen {
            let _ = db::set_season_pattern(&conn, entry_id, s, &pattern_str, season_width as i32, ep_width as i32);
        }
        drop(conn);
        if !poster_source.trim().is_empty() {
            let _ = save_poster(entry_id, &poster_source);
        }

        let msg = format!("Added series with {total} episode(s) found automatically.");
        let app3 = app2.clone();
        let ui_weak2 = ui_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak2.upgrade() {
                refresh_sidebar(&app3, &ui);
                let b = ui.global::<Backend>();
                b.set_add_dialog_open(false);
                b.set_add_in_progress(false);
                b.set_status_text(msg.into());
                select_entry(&app3, &ui, entry_id, "series");
            }
        });
    });
}

// ---------------------------------------------------------------------
// Edit flow — entry title/description/link, or a single episode
// ---------------------------------------------------------------------

// Always edits the whole entry - the movie or the series itself - no
// matter whether some episode happens to be selected inside it at the
// moment.
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
    b.set_edit_poster_path("".into());
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
    let poster_source = b.get_edit_poster_path().to_string();

    let target = app.pending_edit.lock().unwrap().clone();
    let conn = app.conn.lock().unwrap();
    let result = match &target {
        Some(EditTarget::Entry(id)) => db::update_entry(&conn, *id, &title, &description, &link),
        Some(EditTarget::Episode(id)) => db::update_episode(&conn, *id, &title, &description, &link),
        None => Ok(()),
    };
    drop(conn);

    if let (Ok(()), Some(EditTarget::Entry(id))) = (&result, &target) {
        if !poster_source.trim().is_empty() {
            let _ = save_poster(*id, &poster_source);
        }
    }

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

// Fires from the "Delete" button in the entry header. This always targets
// the whole entry - movie or entire series - even if some individual
// episode happens to be selected at the time.
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

// Fires from the edit/delete icons on an individual timeline episode.
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

// Fires from the "Delete season" button on a season header.
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
                    delete_poster_files(id);
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
        Some(DeleteTarget::BulkEpisodes(ids)) => {
            if ids.is_empty() {
                return;
            }
            let entry_id = {
                let conn = app.conn.lock().unwrap();
                db::get_episode(&conn, ids[0]).ok().map(|e| e.entry_id)
            };

            let conn = app.conn.lock().unwrap();
            if let Ok(Some(lp)) = db::get_last_played(&conn) {
                if let Some(eid) = lp.episode_id {
                    if ids.contains(&eid) {
                        let _ = db::clear_last_played(&conn);
                    }
                }
            }
            let count = ids.len();
            for id in &ids {
                let _ = db::delete_episode(&conn, *id);
            }
            drop(conn);

            app.bulk_selected.lock().unwrap().clear();
            ui.global::<Backend>().set_bulk_select_mode(false);
            refresh_last_played(app, ui);
            if let Some(entry_id) = entry_id {
                select_entry(app, ui, entry_id, "series");
            }
            b.set_status_text(format!("Deleted {count} episode(s).").into());
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
    b.set_add_season_in_progress(false);
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
    b.set_add_season_in_progress(true);
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
                set_add_season_in_progress_from_thread(&ui_weak, false);
                return;
            }
        };
        let total = rows.len();

        let conn = app2.conn.lock().unwrap();
        for (s, ep, link) in &rows {
            let ep_title = format!("Episode {ep}");
            let _ = db::insert_episode(&conn, entry_id, *s, *ep, &ep_title, "", link);
        }
        let _ = db::set_season_pattern(&conn, entry_id, season, &pattern_str, season_width as i32, ep_width as i32);
        drop(conn);

        let msg = format!("Added season {season} with {total} episode(s).");
        let app3 = app2.clone();
        let ui_weak2 = ui_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak2.upgrade() {
                let b = ui.global::<Backend>();
                b.set_add_season_dialog_open(false);
                b.set_add_season_in_progress(false);
                b.set_status_text(msg.into());
                select_entry(&app3, &ui, entry_id, "series");
            }
        });
    });
}

// Runs the same auto-detection that created this season in the first
// place, just starting right after its current last episode - so a newly
// published episode gets picked up without anyone retyping the link
// pattern. Only works for seasons that were actually added via a pattern
// (through the initial series creation or "Add season"); ones added by
// hand have nothing saved to re-probe with.
fn request_reprobe_season(app: &Arc<AppState>, ui: &MainWindow, season: i32) {
    let entry_id = match &*app.selected.lock().unwrap() {
        Selection::Series { entry_id, .. } => *entry_id,
        _ => return,
    };
    let conn = app.conn.lock().unwrap();
    let pattern_info = db::get_season_pattern(&conn, entry_id, season).ok().flatten();
    let episodes = db::list_episodes(&conn, entry_id).unwrap_or_default();
    drop(conn);

    let Some((pattern_str, season_width, ep_width)) = pattern_info else {
        ui.global::<Backend>()
            .set_status_text(format!("No saved link pattern for season {season} - can't re-probe. Add episodes manually, or delete and re-add this season with a pattern.").into());
        return;
    };
    let next_ep = episodes
        .iter()
        .filter(|e| e.season == season)
        .map(|e| e.episode)
        .max()
        .map(|m| m + 1)
        .unwrap_or(1);

    let b = ui.global::<Backend>();
    b.set_status_text(format!("Re-probing season {season} for new episodes...").into());
    let app2 = app.clone();
    let ui_weak = ui.as_weak();
    thread::spawn(move || {
        let progress_ui_weak = ui_weak.clone();
        let rows = pattern::probe_season(
            &pattern_str,
            season,
            season_width as usize,
            next_ep,
            ep_width as usize,
            move |s, ep, ok| {
                if ok {
                    set_status_from_thread(&progress_ui_weak, format!("Found S{s:02}E{ep:02}, checking next..."));
                }
            },
        );

        let rows = match rows {
            Ok(v) => v,
            Err(_) => {
                // Nothing past the last known episode yet - that's just the
                // normal "no new episodes" case, not something gone wrong.
                set_status_from_thread(&ui_weak, "No new episodes found.".to_string());
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

        let msg = format!("Found {total} new episode(s) in season {season}.");
        let app3 = app2.clone();
        let ui_weak2 = ui_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak2.upgrade() {
                ui.global::<Backend>().set_status_text(msg.into());
                select_entry(&app3, &ui, entry_id, "series");
            }
        });
    });
}

// ---------------------------------------------------------------------
// Bulk episode actions (multi-select delete / mark watched-unwatched)
// ---------------------------------------------------------------------

fn toggle_bulk_select_mode(app: &Arc<AppState>, ui: &MainWindow) {
    let b = ui.global::<Backend>();
    let new_mode = !b.get_bulk_select_mode();
    b.set_bulk_select_mode(new_mode);
    if !new_mode {
        app.bulk_selected.lock().unwrap().clear();
    }
    b.set_bulk_selected_count(0);
    refresh_current_series_timeline(app, ui);
}

fn toggle_bulk_selected(app: &Arc<AppState>, ui: &MainWindow, episode_id: i64) {
    let count = {
        let mut set = app.bulk_selected.lock().unwrap();
        if !set.insert(episode_id) {
            set.remove(&episode_id);
        }
        set.len()
    };
    ui.global::<Backend>().set_bulk_selected_count(count as i32);
    refresh_current_series_timeline(app, ui);
}

fn bulk_delete_selected(app: &Arc<AppState>, ui: &MainWindow) {
    let ids: Vec<i64> = app.bulk_selected.lock().unwrap().iter().copied().collect();
    if ids.is_empty() {
        return;
    }
    *app.pending_delete.lock().unwrap() = Some(DeleteTarget::BulkEpisodes(ids.clone()));
    let b = ui.global::<Backend>();
    b.set_confirm_message(format!("Delete {} selected episode(s)? This cannot be undone.", ids.len()).into());
    b.set_confirm_dialog_open(true);
}

fn bulk_set_finished(app: &Arc<AppState>, ui: &MainWindow, finished: bool) {
    let entry_id = match &*app.selected.lock().unwrap() {
        Selection::Series { entry_id, .. } => *entry_id,
        _ => return,
    };
    let ids: Vec<i64> = app.bulk_selected.lock().unwrap().iter().copied().collect();
    if ids.is_empty() {
        return;
    }
    let conn = app.conn.lock().unwrap();
    for id in &ids {
        if let Ok(ep) = db::get_episode(&conn, *id) {
            let pos = if finished { ep.duration.max(0) } else { 0 };
            let _ = db::update_episode_resume(&conn, *id, pos, ep.duration, finished);
        }
    }
    drop(conn);

    let ui_backend = ui.global::<Backend>();
    ui_backend.set_status_text(
        format!("Marked {} episode(s) as {}.", ids.len(), if finished { "watched" } else { "unwatched" }).into(),
    );
    refresh_last_played(app, ui);
    refresh_current_series_timeline(app, ui);
    let _ = entry_id;
}

// A manual "Mark watched"/"Mark unwatched" switch for movies, separate from
// the automatic 90%-runtime rule - useful for when playback crashed, or the
// movie got watched somewhere else entirely.
fn toggle_movie_watched(app: &Arc<AppState>, ui: &MainWindow) {
    let id = match &*app.selected.lock().unwrap() {
        Selection::Movie(id) => *id,
        _ => return,
    };
    let conn = app.conn.lock().unwrap();
    let entry = match db::get_entry(&conn, id) {
        Ok(e) => e,
        Err(_) => return,
    };
    let finished = !entry.finished;
    let pos = if finished { entry.duration.max(0) } else { 0 };
    let _ = db::update_entry_resume(&conn, id, pos, entry.duration, finished);
    drop(conn);

    select_entry(app, ui, id, "movie");
    ui.global::<Backend>()
        .set_status_text(format!("Marked as {}.", if finished { "watched" } else { "unwatched" }).into());
}

// ---------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------

fn open_settings_dialog(app: &Arc<AppState>, ui: &MainWindow) {
    let conn = app.conn.lock().unwrap();
    let path = db::get_vlc_path(&conn).ok().flatten().unwrap_or_default();
    let subtitle_lang = db::get_subtitle_lang(&conn).ok().flatten().unwrap_or_default();
    drop(conn);

    let b = ui.global::<Backend>();
    b.set_settings_vlc_path(path.into());
    b.set_settings_subtitle_lang(subtitle_lang.into());
    b.set_status_text("".into());
    b.set_settings_dialog_open(true);
}

fn submit_settings(app: &Arc<AppState>, ui: &MainWindow) {
    let b = ui.global::<Backend>();
    let path = b.get_settings_vlc_path().to_string();
    let subtitle_lang = b.get_settings_subtitle_lang().to_string();

    let conn = app.conn.lock().unwrap();
    let result = db::set_vlc_path(&conn, &path).and_then(|_| db::set_subtitle_lang(&conn, &subtitle_lang));
    drop(conn);

    match result {
        Ok(()) => {
            b.set_settings_dialog_open(false);
            b.set_status_text("Settings saved.".into());
        }
        Err(e) => b.set_status_text(format!("Failed to save settings: {e}").into()),
    }
}

// ---------------------------------------------------------------------
// Data management: export/import (JSON) and a link health check
// ---------------------------------------------------------------------

// Puts together a plain JSON snapshot of the whole library - titles,
// descriptions, links, watched/progress state. Poster images don't come
// along for the ride; this is meant for backing things up or moving them
// between machines, not a full clone.
fn export_library_json(app: &Arc<AppState>) -> anyhow::Result<serde_json::Value> {
    let conn = app.conn.lock().unwrap();
    let movies = db::list_entries(&conn, EntryKind::Movie)?;
    let series = db::list_entries(&conn, EntryKind::Series)?;

    let movies_json: Vec<serde_json::Value> = movies
        .iter()
        .map(|e| {
            serde_json::json!({
                "title": e.title,
                "description": e.description,
                "link_or_path": e.link_or_path,
                "resume_position": e.resume_position,
                "duration": e.duration,
                "finished": e.finished,
            })
        })
        .collect();

    let mut series_json = Vec::new();
    for s in &series {
        let episodes = db::list_episodes(&conn, s.id)?;
        let episodes_json: Vec<serde_json::Value> = episodes
            .iter()
            .map(|ep| {
                serde_json::json!({
                    "season": ep.season,
                    "episode": ep.episode,
                    "title": ep.title,
                    "description": ep.description,
                    "link_or_path": ep.link_or_path,
                    "resume_position": ep.resume_position,
                    "duration": ep.duration,
                    "finished": ep.finished,
                })
            })
            .collect();
        series_json.push(serde_json::json!({
            "title": s.title,
            "description": s.description,
            "episodes": episodes_json,
        }));
    }
    drop(conn);

    Ok(serde_json::json!({
        "format": "aparatchi-library-v1",
        "movies": movies_json,
        "series": series_json,
    }))
}

fn export_library(app: &Arc<AppState>, ui: &MainWindow) {
    let Some(path) = rfd::FileDialog::new()
        .set_file_name("aparatchi-library.json")
        .add_filter("JSON", &["json"])
        .save_file()
    else {
        return;
    };
    let b = ui.global::<Backend>();
    match export_library_json(app) {
        Ok(value) => {
            let content = serde_json::to_string_pretty(&value).unwrap_or_default();
            match std::fs::write(&path, content) {
                Ok(()) => b.set_status_text("Library exported.".into()),
                Err(e) => b.set_status_text(format!("Export failed: {e}").into()),
            }
        }
        Err(e) => b.set_status_text(format!("Export failed: {e}").into()),
    }
}

// Reads entries back in from a previously exported JSON file. Everything
// comes in as brand-new entries - we don't try to match ids against what's
// already in the library - so importing the same file twice will just give
// you two copies of everything. Think "restore into an empty library" or
// "merge in a set I know is different," not a proper two-way sync.
fn import_library_json(conn: &Connection, value: &serde_json::Value) -> anyhow::Result<(usize, usize)> {
    let mut movie_count = 0;
    if let Some(movies) = value.get("movies").and_then(|v| v.as_array()) {
        for m in movies {
            let title = m.get("title").and_then(|v| v.as_str()).unwrap_or("Untitled");
            let description = m.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let link = m.get("link_or_path").and_then(|v| v.as_str()).unwrap_or("");
            if let Ok(id) = db::insert_entry(conn, title, EntryKind::Movie, description, link) {
                let resume_position = m.get("resume_position").and_then(|v| v.as_i64()).unwrap_or(0);
                let duration = m.get("duration").and_then(|v| v.as_i64()).unwrap_or(0);
                let finished = m.get("finished").and_then(|v| v.as_bool()).unwrap_or(false);
                let _ = db::update_entry_resume(conn, id, resume_position, duration, finished);
                movie_count += 1;
            }
        }
    }

    let mut series_count = 0;
    if let Some(series_list) = value.get("series").and_then(|v| v.as_array()) {
        for s in series_list {
            let title = s.get("title").and_then(|v| v.as_str()).unwrap_or("Untitled");
            let description = s.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let Ok(entry_id) = db::insert_entry(conn, title, EntryKind::Series, description, "") else {
                continue;
            };
            series_count += 1;
            if let Some(episodes) = s.get("episodes").and_then(|v| v.as_array()) {
                for ep in episodes {
                    let season = ep.get("season").and_then(|v| v.as_i64()).unwrap_or(1) as i32;
                    let episode = ep.get("episode").and_then(|v| v.as_i64()).unwrap_or(1) as i32;
                    let ep_title = ep.get("title").and_then(|v| v.as_str()).unwrap_or("");
                    let ep_desc = ep.get("description").and_then(|v| v.as_str()).unwrap_or("");
                    let ep_link = ep.get("link_or_path").and_then(|v| v.as_str()).unwrap_or("");
                    if let Ok(id) = db::insert_episode(conn, entry_id, season, episode, ep_title, ep_desc, ep_link) {
                        let resume_position = ep.get("resume_position").and_then(|v| v.as_i64()).unwrap_or(0);
                        let duration = ep.get("duration").and_then(|v| v.as_i64()).unwrap_or(0);
                        let finished = ep.get("finished").and_then(|v| v.as_bool()).unwrap_or(false);
                        let _ = db::update_episode_resume(conn, id, resume_position, duration, finished);
                    }
                }
            }
        }
    }

    Ok((movie_count, series_count))
}

fn import_library(app: &Arc<AppState>, ui: &MainWindow) {
    let Some(path) = rfd::FileDialog::new().add_filter("JSON", &["json"]).pick_file() else {
        return;
    };
    let b = ui.global::<Backend>();
    let result = std::fs::read_to_string(&path)
        .map_err(anyhow::Error::from)
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).map_err(anyhow::Error::from))
        .and_then(|value| {
            let conn = app.conn.lock().unwrap();
            import_library_json(&conn, &value)
        });

    match result {
        Ok((movies, series)) => {
            refresh_sidebar(app, ui);
            b.set_status_text(format!("Imported {movies} movie(s) and {series} series.").into());
        }
        Err(e) => b.set_status_text(format!("Import failed: {e}").into()),
    }
}

// Goes through every stored movie/episode link in the background (reusing
// the same check the auto-detection uses) and reports back how many turned
// out broken.
fn check_all_links(app: &Arc<AppState>, ui: &MainWindow) {
    ui.global::<Backend>().set_status_text("Checking links...".into());
    let app2 = app.clone();
    let ui_weak = ui.as_weak();
    thread::spawn(move || {
        let conn = app2.conn.lock().unwrap();
        let movies = db::list_entries(&conn, EntryKind::Movie).unwrap_or_default();
        let series = db::list_entries(&conn, EntryKind::Series).unwrap_or_default();

        let mut items: Vec<(String, String)> = Vec::new();
        for m in &movies {
            if !m.link_or_path.trim().is_empty() {
                items.push((m.title.clone(), m.link_or_path.clone()));
            }
        }
        for s in &series {
            let episodes = db::list_episodes(&conn, s.id).unwrap_or_default();
            for ep in &episodes {
                if !ep.link_or_path.trim().is_empty() {
                    items.push((format!("{} - {}", s.title, ep.label()), ep.link_or_path.clone()));
                }
            }
        }
        drop(conn);

        let total = items.len();
        if total == 0 {
            set_status_from_thread(&ui_weak, "No links to check.".to_string());
            return;
        }

        let mut broken: Vec<String> = Vec::new();
        for (i, (label, link)) in items.iter().enumerate() {
            if !pattern::verify(link) {
                broken.push(label.clone());
            }
            if (i + 1) % 5 == 0 || i + 1 == total {
                set_status_from_thread(&ui_weak, format!("Checking links... {}/{total}", i + 1));
            }
        }

        let msg = if broken.is_empty() {
            format!("Checked {total} link(s). All resolved fine.")
        } else {
            let preview: Vec<_> = broken.iter().take(5).cloned().collect();
            let mut m = format!("Checked {total} link(s) - {} broken: {}", broken.len(), preview.join(", "));
            if broken.len() > 5 {
                m.push_str(&format!(", and {} more.", broken.len() - 5));
            }
            m
        };
        set_status_from_thread(&ui_weak, msg);
    });
}
