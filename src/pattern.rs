use anyhow::{anyhow, Result};
use std::time::Duration;

/// Safety caps so a pattern/server that never actually 404s
/// can't make the probe loop forever.
const MAX_SEASONS: i32 = 500;
const MAX_EPISODES_PER_SEASON: i32 = 2000;

/// Substitute season/episode numbers into a pattern. Both `#` (season) and
/// `*` (episode) may appear more than once in the pattern. e.g.
/// `http://example.com/S#/movie.S#E*.mkv` -> every occurrence is replaced.
fn render(pattern: &str, season: i32, season_width: usize, episode: i32, ep_width: usize) -> String {
    let season_str = format!("{:0width$}", season, width = season_width.max(1));
    let ep_str = format!("{:0width$}", episode, width = ep_width.max(1));
    pattern.replace('#', &season_str).replace('*', &ep_str)
}

/// Best-effort check that a generated link actually resolves: HTTP HEAD for
/// URLs (treated as "not found" on any non-2xx/405 status, including 404),
/// filesystem existence check for local paths.
pub fn verify(link: &str) -> bool {
    if link.starts_with("http://") || link.starts_with("https://") {
        let client = match reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(6))
            .build()
        {
            Ok(c) => c,
            Err(_) => return false,
        };
        client
            .head(link)
            .send()
            .map(|r| r.status().is_success() || r.status().as_u16() == 405) // some servers reject HEAD
            .unwrap_or(false)
    } else {
        std::path::Path::new(link).exists()
    }
}

/// Probe a single season: starting at `ep_start`, keep incrementing the
/// episode number (substituted into every `*`, and every `#` if present)
/// and verifying the resulting link until one fails to resolve.
fn probe_season_episodes<F: FnMut(i32, i32, bool)>(
    pattern: &str,
    season: i32,
    season_width: usize,
    ep_start: i32,
    ep_width: usize,
    on_probe: &mut F,
) -> Vec<(i32, i32, String)> {
    let mut out = Vec::new();
    let mut ep = ep_start;
    loop {
        let link = render(pattern, season, season_width, ep, ep_width);
        let ok = verify(&link);
        on_probe(season, ep, ok);
        if !ok {
            break;
        }
        out.push((season, ep, link));
        ep += 1;
        if ep - ep_start >= MAX_EPISODES_PER_SEASON {
            break;
        }
    }
    out
}

/// Discover a series' season/episode range automatically instead of asking
/// for an end value: starting at `ep_start`, keep incrementing the episode
/// number until one fails to resolve -> that's the end of the season. If the
/// pattern also contains `#`, the same thing happens one season at a time
/// starting at `season_start`, stopping as soon as a season's very first
/// episode fails to verify. If the pattern has no `#`, only a single season
/// (`season_start`) is probed.
///
/// `on_probe` is called after every attempt (`season`, `episode`, `verified`)
/// so callers can surface live progress.
///
/// Returns one `(season, episode, link)` row per verified episode found, in
/// season-then-episode order.
pub fn probe<F: FnMut(i32, i32, bool)>(
    pattern: &str,
    season_start: i32,
    season_width: usize,
    ep_start: i32,
    ep_width: usize,
    mut on_probe: F,
) -> Result<Vec<(i32, i32, String)>> {
    if !pattern.contains('*') {
        return Err(anyhow!("pattern must contain a '*' placeholder for the episode number"));
    }
    let multi_season = pattern.contains('#');

    let mut out = Vec::new();
    let mut season = season_start;
    loop {
        let season_rows = probe_season_episodes(pattern, season, season_width, ep_start, ep_width, &mut on_probe);
        let found = season_rows.len();
        out.extend(season_rows);

        if !multi_season || found == 0 {
            // Either this pattern only ever describes one season, or this
            // season's first episode didn't verify -> treat that as the
            // "404" marking the end of the series' season range.
            break;
        }
        season += 1;
        if season - season_start >= MAX_SEASONS {
            break;
        }
    }

    if out.is_empty() {
        return Err(anyhow!(
            "no episodes found — the very first link (season {season_start}, episode {ep_start}) didn't verify. Check the pattern and starting numbers."
        ));
    }
    Ok(out)
}

/// Same auto-detection as [`probe`], but pinned to exactly one season.
/// used for adding a single new season (potentially with its own distinct
/// link pattern) to an existing series.
pub fn probe_season<F: FnMut(i32, i32, bool)>(
    pattern: &str,
    season: i32,
    season_width: usize,
    ep_start: i32,
    ep_width: usize,
    mut on_probe: F,
) -> Result<Vec<(i32, i32, String)>> {
    if !pattern.contains('*') {
        return Err(anyhow!("pattern must contain a '*' placeholder for the episode number"));
    }
    let out = probe_season_episodes(pattern, season, season_width, ep_start, ep_width, &mut on_probe);
    if out.is_empty() {
        return Err(anyhow!(
            "no episodes found — the first link (season {season}, episode {ep_start}) didn't verify. Check the pattern and starting episode number."
        ));
    }
    Ok(out)
}
