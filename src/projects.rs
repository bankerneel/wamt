//! Per-project rollups: a project is a T3 project title, or the working
//! folder's name for sessions T3 does not know. Only names and numbers are
//! kept here, never paths or message content.
//!
//! Each day keeps at most twelve named projects by weight; anything smaller
//! is folded into "Other" so the history file stays small.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::BTreeMap;

/// Sessions with no T3 project and no folder name land here.
pub const NO_PROJECT: &str = "No project";
/// Everything outside a day's top twelve.
pub const OTHER: &str = "Other";
/// Named projects kept per day, before "Other".
pub const PER_DAY: usize = 12;

/// Weighted units, requests and output tokens for one project on one day.
/// Weight is list-price micro-dollars; Codex has weight 0 and counts tokens.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjUse {
    pub w: f64,
    pub n: u64,
    pub out: u64,
}

pub type Map = BTreeMap<String, ProjUse>;
/// Alias used where the map is one field of a day or payload.
pub type ProjectMap = Map;

/// Adds one request to a project.
pub fn add(map: &mut Map, project: &str, w: f64, n: u64, out: u64) {
    let e = map.entry(project.to_string()).or_default();
    e.w += w;
    e.n += n;
    e.out += out;
}

/// The basename of a working folder. Callers pass the metadata folder name
/// the tailer already reduced; a stray full path is reduced again here so it
/// can never reach a payload.
pub fn folder_name(p: &str) -> &str {
    p.trim_end_matches('/').rsplit('/').next().unwrap_or(p)
}

/// T3 threads are named by their project title, everything else by the
/// session's working-folder name. Missing metadata becomes "No project".
pub fn attribute(is_t3: bool, t3_project: Option<&str>, cwd: Option<&str>) -> String {
    if is_t3 {
        return t3_project.filter(|s| !s.is_empty()).unwrap_or(NO_PROJECT).to_string();
    }
    cwd.map(folder_name)
        .filter(|s| !s.is_empty())
        .unwrap_or(NO_PROJECT)
        .to_string()
}

/// Keeps the twelve heaviest projects and folds the rest into "Other".
/// Ties break by name so a saved day never changes order between runs.
pub fn fold(full: &Map) -> Map {
    let mut ranked: Vec<(&String, &ProjUse)> = full.iter().filter(|(k, _)| k.as_str() != OTHER).collect();
    ranked.sort_by(|a, b| b.1.w.partial_cmp(&a.1.w).unwrap_or(Ordering::Equal).then_with(|| a.0.cmp(b.0)));
    let mut out = Map::new();
    for (name, u) in ranked.into_iter().take(PER_DAY) {
        out.insert(name.clone(), u.clone());
    }
    let mut other = full.get(OTHER).cloned().unwrap_or_default();
    for (name, u) in full {
        if name != OTHER && !out.contains_key(name) {
            other.w += u.w;
            other.n += u.n;
            other.out += u.out;
        }
    }
    if other.w > 0.0 || other.n > 0 || other.out > 0 {
        out.insert(OTHER.to_string(), other);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t3_project_beats_folder_name_and_missing_metadata_is_no_project() {
        // A T3 thread is named by its project title even when the session
        // carries a different working folder.
        assert_eq!(attribute(true, Some("demo-app"), Some("/Users/x/demo")), "demo-app");
        // T3 threads without a title have no project at all.
        assert_eq!(attribute(true, None, Some("demo")), NO_PROJECT);
        // Anything else uses the folder NAME, never the full path.
        assert_eq!(attribute(false, Some("ignored"), Some("/Users/x/projects/demo/")), "demo");
        assert_eq!(attribute(false, None, None), NO_PROJECT);
        assert_eq!(attribute(false, None, Some("")), NO_PROJECT);
    }

    #[test]
    fn top_twelve_are_kept_and_the_rest_become_other() {
        let mut full = Map::new();
        for i in 0..15 {
            add(&mut full, &format!("p{i:02}"), (15 - i) as f64, 1, 100);
        }
        let folded = fold(&full);
        assert_eq!(folded.len(), PER_DAY + 1);
        for i in 0..PER_DAY {
            assert!(folded.contains_key(&format!("p{i:02}")));
        }
        assert!(!folded.contains_key("p12"));
        let other = &folded[OTHER];
        assert_eq!(other.w, 3.0 + 2.0 + 1.0);
        assert_eq!(other.n, 3);
        assert_eq!(other.out, 300);
        let sum: f64 = folded.values().map(|u| u.w).sum();
        assert_eq!(sum, full.values().map(|u| u.w).sum::<f64>());
        // Folding twice is stable.
        assert_eq!(fold(&folded), folded);
    }
}
