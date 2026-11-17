//! The only files Switchboard writes: small JSON caches of offsets and
//! numbers in the state directory. Written atomically (temp file + rename).
//! Deleting them is always safe; the next start re-reads the 3-day window.

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

pub fn save<T: Serialize>(path: &Path, value: &T) -> io::Result<usize> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(bytes.len())
}

pub fn load<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn round_trip_and_atomic_replace() {
        let dir = std::env::temp_dir().join(format!("sb-state-{}", std::process::id()));
        let p = dir.join("x.json");
        let mut m = HashMap::new();
        m.insert("a".to_string(), 1u32);
        save(&p, &m).unwrap();
        m.insert("b".to_string(), 2);
        save(&p, &m).unwrap();
        let back: HashMap<String, u32> = load(&p).unwrap();
        assert_eq!(back, m);
        assert!(!p.with_extension("json.tmp").exists());
        assert!(load::<HashMap<String, u32>>(&dir.join("missing.json")).is_none());
        fs::remove_dir_all(dir).ok();
    }
}
