#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    Movie,
    Series,
}

impl EntryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EntryKind::Movie => "movie",
            EntryKind::Series => "series",
        }
    }

    pub fn from_str(s: &str) -> Self {
        if s == "series" {
            EntryKind::Series
        } else {
            EntryKind::Movie
        }
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub id: i64,
    pub title: String,
    pub kind: EntryKind,
    pub description: String,
    /// For movies: the file path or URL to play.
    /// For series: unused for playback (kept as an optional reference link).
    pub link_or_path: String,
    pub resume_position: i64, // seconds; only meaningful for movies
    pub duration: i64,        // seconds; last known length
    /// Reached the final ~10% of its runtime the last time it was played.
    pub finished: bool,
}

#[derive(Debug, Clone)]
pub struct Episode {
    pub id: i64,
    pub entry_id: i64,
    pub season: i32,
    pub episode: i32,
    pub title: String,
    pub description: String,
    pub link_or_path: String,
    pub resume_position: i64,
    pub duration: i64,
    /// Reached the final ~10% of its runtime the last time it was played.
    pub finished: bool,
}

impl Episode {
    pub fn label(&self) -> String {
        let base = format!("S{:02}E{:02}", self.season, self.episode);
        if self.title.trim().is_empty() {
            base
        } else {
            format!("{base} - {}", self.title)
        }
    }
}
