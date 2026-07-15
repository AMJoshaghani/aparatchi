use anyhow::{anyhow, Result};
use std::time::Duration;

// Just a safety net in case a pattern or server never actually 404s (some
// sites return 200 for literally everything) - without this, the probe
// loop would just run forever.
const MAX_SEASONS: i32 = 500;
const MAX_EPISODES_PER_SEASON: i32 = 2000;

// Fills a season/episode number into a pattern. Both `#` (season) and `*`
// (episode) can show up more than once in the same pattern - something
// like `http://example.com/S#/movie.S#E*.mkv` - and every occurrence gets
// replaced, not just the first.
fn render(pattern: &str, season: i32, season_width: usize, episode: i32, ep_width: usize) -> String {
    let season_str = format!("{:0width$}", season, width = season_width.max(1));
    let ep_str = format!("{:0width$}", episode, width = ep_width.max(1));
    pattern.replace('#', &season_str).replace('*', &ep_str)
}

// Checks whether a generated link actually resolves to something. For
// URLs that's an HTTP HEAD request (anything other than a 2xx or 405 counts
// as "not found", 404 included); for local paths it's just a filesystem
// check.
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

// Walks through a single season: start at `ep_start` and keep bumping the
// episode number (filled into every `*`, and every `#` too if the pattern
// has one), checking each generated link, until one of them fails to
// resolve.
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

// This is how we figure out a series' season/episode range without ever
// asking the user for an end value: start at `ep_start` and keep bumping
// the episode number until one fails to resolve - that's the end of the
// season. If the pattern also has a `#` in it, we do the same thing one
// season at a time starting from `season_start`, and stop as soon as a
// season's very first episode doesn't verify. No `#` in the pattern just
// means there's only the one season (`season_start`) to probe.
//
// `on_probe` gets called after every single attempt with (season, episode,
// verified), so whoever's calling this can show live progress as it goes.
//
// What comes back is one (season, episode, link) row per episode that
// actually verified, in season-then-episode order.
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
            // Either this pattern only ever covers one season to begin with,
            // or this season's first episode just didn't verify - either way,
            // treat it as the "404" that marks the end of the series.
            break;
        }
        season += 1;
        if season - season_start >= MAX_SEASONS {
            break;
        }
    }

    if out.is_empty() {
        return Err(anyhow!(
            "no episodes found - the very first link (season {season_start}, episode {ep_start}) didn't verify. Check the pattern and starting numbers."
        ));
    }
    Ok(out)
}

// The same auto-detection [`probe`] does, just pinned to exactly one
// season - this is what we use for adding a single new season (which might
// have its own, completely different link pattern) to a series that
// already exists.
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
            "no episodes found - the first link (season {season}, episode {ep_start}) didn't verify. Check the pattern and starting episode number."
        ));
    }
    Ok(out)
}
