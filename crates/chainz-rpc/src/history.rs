//! Persistent per-endpoint health history (`<state_dir>/health.json`).

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::capabilities::Capabilities;
use crate::error::ErrorClass;
use crate::probe::ProbeResult;

const FILE_NAME: &str = "health.json";
/// Where a `health.json` that does not parse is moved, so that it is never
/// overwritten unseen.
const CORRUPT_NAME: &str = "health.json.corrupt";
const LOCK_NAME: &str = "health.lock";
const FORMAT_VERSION: u32 = 1;

/// One recorded probe outcome.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HealthSample {
    /// When the sample was taken, in unix milliseconds.
    pub at_ms: u64,
    /// Probe latency in milliseconds (saturating).
    pub latency_ms: u32,
    /// Head block, when the probe succeeded.
    pub head: Option<u64>,
    /// Error class, when the probe failed.
    pub error: Option<ErrorClass>,
}

impl HealthSample {
    /// A sample from a probe result, timestamped now.
    pub fn from_probe(result: &ProbeResult) -> Self {
        HealthSample {
            at_ms: chainz_os::now_ms(),
            latency_ms: u32::try_from(result.latency.as_millis()).unwrap_or(u32::MAX),
            head: result.head,
            error: result.error.as_ref().map(|e| e.class()),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Entry {
    samples: VecDeque<HealthSample>,
    capabilities: Option<Capabilities>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct FileFormat {
    version: u32,
    endpoints: Vec<FileEntry>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct FileEntry {
    chain_id: u64,
    endpoint: String,
    #[serde(default)]
    samples: Vec<HealthSample>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    capabilities: Option<Capabilities>,
}

type Key = (u64, String);

/// Ring buffer ([`HealthStore::CAPACITY`] samples) per (chain ID, endpoint
/// name) plus cached capabilities, persisted as `<state_dir>/health.json`.
///
/// Saving merges with whatever other processes wrote in the meantime, so
/// concurrent writers do not lose each other's samples.
#[derive(Debug, Clone)]
pub struct HealthStore {
    /// Where it is saved; `None` for a store with nowhere to go.
    dir: Option<PathBuf>,
    entries: BTreeMap<Key, Entry>,
}

impl HealthStore {
    /// Samples kept per endpoint.
    pub const CAPACITY: usize = 64;

    /// The file a store in `state_dir` lives in.
    pub fn path(state_dir: &Path) -> PathBuf {
        state_dir.join(FILE_NAME)
    }

    /// Load the store from `state_dir`. A missing file is an empty store.
    /// A file that does not parse is moved aside to `health.json.corrupt`
    /// (so no save overwrites it unseen) and reported as an
    /// [`io::ErrorKind::InvalidData`] error; other I/O errors (e.g.
    /// permissions) are returned as they are. After an error, a caller that
    /// carries on uses [`HealthStore::new`].
    pub fn load(state_dir: &Path) -> io::Result<Self> {
        let path = Self::path(state_dir);
        let entries = match read_entries(&path)? {
            Some(entries) => entries,
            None => {
                // Re-checked under the lock: another process may have just
                // replaced the file with a good one.
                let lock = lock(state_dir)?;
                match read_entries(&path)? {
                    Some(entries) => entries,
                    None => {
                        let aside = move_aside(state_dir)?;
                        drop(lock);
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "{} did not parse; moved it to {}",
                                path.display(),
                                aside.display()
                            ),
                        ));
                    }
                }
            }
        };
        Ok(HealthStore {
            dir: Some(state_dir.to_path_buf()),
            entries,
        })
    }

    /// An empty store that saves to `state_dir` (merging with what is there
    /// by then).
    pub fn new(state_dir: &Path) -> Self {
        HealthStore {
            dir: Some(state_dir.to_path_buf()),
            entries: BTreeMap::new(),
        }
    }

    /// An empty store that is never saved ([`save`](Self::save) fails).
    pub fn empty() -> Self {
        HealthStore {
            dir: None,
            entries: BTreeMap::new(),
        }
    }

    /// Append a sample, evicting the oldest beyond [`Self::CAPACITY`].
    pub fn record(&mut self, chain_id: u64, endpoint: &str, sample: HealthSample) {
        let entry = self
            .entries
            .entry((chain_id, endpoint.to_string()))
            .or_default();
        entry.samples.push_back(sample);
        while entry.samples.len() > Self::CAPACITY {
            entry.samples.pop_front();
        }
    }

    /// All samples for an endpoint, oldest first.
    pub fn samples(&self, chain_id: u64, endpoint: &str) -> Vec<HealthSample> {
        self.entry(chain_id, endpoint)
            .map(|e| e.samples.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// The most recent sample for an endpoint.
    pub fn latest(&self, chain_id: u64, endpoint: &str) -> Option<&HealthSample> {
        self.entry(chain_id, endpoint)?.samples.back()
    }

    /// Cached capabilities for an endpoint.
    pub fn capabilities(&self, chain_id: u64, endpoint: &str) -> Option<&Capabilities> {
        self.entry(chain_id, endpoint)?.capabilities.as_ref()
    }

    /// Replace the cached capabilities for an endpoint.
    pub fn set_capabilities(&mut self, chain_id: u64, endpoint: &str, caps: Capabilities) {
        self.entries
            .entry((chain_id, endpoint.to_string()))
            .or_default()
            .capabilities = Some(caps);
    }

    /// The capability tags to show for an endpoint: the cached ones, else
    /// `ws` for a websocket endpoint (which is never probed).
    pub fn tags(&self, chain_id: u64, endpoint: &str, websocket: bool) -> Vec<String> {
        match self.capabilities(chain_id, endpoint) {
            Some(capabilities) => capabilities.tags(),
            None if websocket => vec!["ws".to_string()],
            None => Vec::new(),
        }
    }

    /// [`save`](Self::save), and on failure the warning to show:
    /// `health history not saved: …`.
    pub fn save_or_warn(&self) -> Option<String> {
        self.save()
            .err()
            .map(|e| format!("health history not saved: {e}"))
    }

    /// Under an exclusive lock file `<state_dir>/health.lock`, re-read the
    /// file, merge this store's samples and capabilities into it, and write
    /// it atomically (tempfile + fsync + rename, mode 0600, dir 0700).
    ///
    /// Merging is idempotent: samples are de-duplicated, ordered by time and
    /// trimmed to [`Self::CAPACITY`]; for capabilities the most recently
    /// probed wins.
    pub fn save(&self) -> io::Result<()> {
        let Some(dir) = &self.dir else {
            return Err(io::Error::other("the health store has no state directory"));
        };
        let _lock = lock(dir)?;
        let path = Self::path(dir);
        let mut merged = match read_entries(&path)? {
            Some(entries) => entries,
            None => {
                move_aside(dir)?;
                BTreeMap::new()
            }
        };
        for (key, ours) in &self.entries {
            let theirs = merged.entry(key.clone()).or_default();
            merge_entry(theirs, ours);
        }
        let file = FileFormat {
            version: FORMAT_VERSION,
            endpoints: merged
                .into_iter()
                .map(|((chain_id, endpoint), e)| FileEntry {
                    chain_id,
                    endpoint,
                    samples: e.samples.into(),
                    capabilities: e.capabilities,
                })
                .collect(),
        };
        let json = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
        chainz_os::write_private(&path, &json)
    }

    fn entry(&self, chain_id: u64, endpoint: &str) -> Option<&Entry> {
        self.entries.get(&(chain_id, endpoint.to_string()))
    }
}

fn merge_entry(into: &mut Entry, ours: &Entry) {
    for s in &ours.samples {
        if !into.samples.contains(s) {
            into.samples.push_back(s.clone());
        }
    }
    into.samples.make_contiguous().sort_by_key(|s| s.at_ms);
    while into.samples.len() > HealthStore::CAPACITY {
        into.samples.pop_front();
    }
    if let Some(caps) = &ours.capabilities {
        let newer = match &into.capabilities {
            None => true,
            Some(existing) => caps.probed_at >= existing.probed_at,
        };
        if newer {
            into.capabilities = Some(caps.clone());
        }
    }
}

/// Take `<state_dir>/health.lock` (creating the directory); released
/// when the returned file closes.
fn lock(state_dir: &Path) -> io::Result<fs::File> {
    chainz_os::lock_file(&state_dir.join(LOCK_NAME))
}

/// Move a `health.json` that does not parse to `health.json.corrupt`
/// (replacing an older one). Call under the lock.
fn move_aside(state_dir: &Path) -> io::Result<PathBuf> {
    let aside = state_dir.join(CORRUPT_NAME);
    fs::rename(HealthStore::path(state_dir), &aside)?;
    Ok(aside)
}

/// The entries in `path`: empty when it does not exist, `None` when it
/// does not parse.
fn read_entries(path: &Path) -> io::Result<Option<BTreeMap<Key, Entry>>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Some(BTreeMap::new())),
        Err(e) => return Err(e),
    };
    let Ok(file) = serde_json::from_slice::<FileFormat>(&bytes) else {
        return Ok(None);
    };
    let mut entries = BTreeMap::new();
    for fe in file.endpoints {
        let mut samples: VecDeque<HealthSample> = fe.samples.into();
        while samples.len() > HealthStore::CAPACITY {
            samples.pop_front();
        }
        entries.insert(
            (fe.chain_id, fe.endpoint),
            Entry {
                samples,
                capabilities: fe.capabilities,
            },
        );
    }
    Ok(Some(entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(at_ms: u64, latency_ms: u32, ok: bool) -> HealthSample {
        HealthSample {
            at_ms,
            latency_ms,
            head: ok.then_some(100),
            error: (!ok).then_some(ErrorClass::Timeout),
        }
    }

    #[test]
    fn ring_buffer_evicts_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = HealthStore::load(dir.path()).unwrap();
        for i in 0..100 {
            store.record(1, "a", sample(i, 10, true));
        }
        let s = store.samples(1, "a");
        assert_eq!(s.len(), HealthStore::CAPACITY);
        assert_eq!(s.first().unwrap().at_ms, 36);
        assert_eq!(store.latest(1, "a").unwrap().at_ms, 99);
        assert!(store.samples(2, "a").is_empty());
        assert!(store.latest(1, "b").is_none());
    }

    #[test]
    fn empty_and_new() {
        let mut empty = HealthStore::empty();
        empty.record(1, "a", sample(1, 5, true));
        assert_eq!(empty.samples(1, "a").len(), 1);
        assert!(empty.save().is_err(), "nowhere to save");

        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("state");
        let mut store = HealthStore::new(&dir);
        store.record(1, "a", sample(1, 5, true));
        store.save().unwrap();
        let loaded = HealthStore::load(&dir).unwrap();
        assert_eq!(loaded.samples(1, "a"), vec![sample(1, 5, true)]);
        assert_eq!(HealthStore::path(&dir), dir.join("health.json"));
    }

    #[test]
    fn save_load_roundtrip_with_modes() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("state");
        let mut store = HealthStore::load(&dir).unwrap();
        store.record(1, "a", sample(1, 5, true));
        let caps = Capabilities {
            archive: Some(true),
            probed_at: Some(7),
            ..Default::default()
        };
        store.set_capabilities(1, "a", caps.clone());
        store.save().unwrap();
        store.save().unwrap(); // idempotent

        let loaded = HealthStore::load(&dir).unwrap();
        assert_eq!(loaded.samples(1, "a"), vec![sample(1, 5, true)]);
        assert_eq!(loaded.capabilities(1, "a"), Some(&caps));

        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir.join(FILE_NAME)), 0o600);
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn a_corrupt_file_is_reported_and_moved_aside_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(FILE_NAME), b"{not json").unwrap();
        let err = HealthStore::load(dir.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains(CORRUPT_NAME), "{err}");
        assert_eq!(
            fs::read(dir.path().join(CORRUPT_NAME)).unwrap(),
            b"{not json"
        );
        let mut store = HealthStore::new(dir.path());
        store.record(1, "a", sample(1, 5, true));
        store.save().unwrap();
        assert_eq!(
            HealthStore::load(dir.path()).unwrap().samples(1, "a").len(),
            1
        );

        // Corrupted behind a running store's back: saving moves it aside too.
        fs::write(dir.path().join(FILE_NAME), b"[]").unwrap();
        store.save().unwrap();
        assert_eq!(fs::read(dir.path().join(CORRUPT_NAME)).unwrap(), b"[]");
        assert_eq!(
            HealthStore::load(dir.path()).unwrap().samples(1, "a").len(),
            1
        );
    }

    #[test]
    fn concurrent_stores_merge_without_losing_samples() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = HealthStore::load(dir.path()).unwrap();
        let mut b = HealthStore::load(dir.path()).unwrap();
        a.record(1, "x", sample(1, 10, true));
        a.record(1, "x", sample(3, 30, true));
        b.record(1, "x", sample(2, 20, true));
        b.record(10, "y", sample(4, 40, false));
        a.set_capabilities(
            1,
            "x",
            Capabilities {
                probed_at: Some(5),
                ..Default::default()
            },
        );
        b.set_capabilities(
            1,
            "x",
            Capabilities {
                probed_at: Some(1),
                archive: Some(true),
                ..Default::default()
            },
        );
        a.save().unwrap();
        b.save().unwrap();

        let merged = HealthStore::load(dir.path()).unwrap();
        let at: Vec<u64> = merged.samples(1, "x").iter().map(|s| s.at_ms).collect();
        assert_eq!(at, vec![1, 2, 3]);
        assert_eq!(merged.samples(10, "y").len(), 1);
        // Newer capabilities (a's, probed_at 5) win over b's older ones.
        assert_eq!(merged.capabilities(1, "x").unwrap().probed_at, Some(5));
    }

    #[test]
    fn merge_trims_to_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = HealthStore::load(dir.path()).unwrap();
        let mut b = HealthStore::load(dir.path()).unwrap();
        for i in 0..50 {
            a.record(1, "x", sample(i * 2, 1, true));
            b.record(1, "x", sample(i * 2 + 1, 1, true));
        }
        a.save().unwrap();
        b.save().unwrap();
        let s = HealthStore::load(dir.path()).unwrap().samples(1, "x");
        assert_eq!(s.len(), HealthStore::CAPACITY);
        assert_eq!(s.first().unwrap().at_ms, 36);
        assert_eq!(s.last().unwrap().at_ms, 99);
    }
}
