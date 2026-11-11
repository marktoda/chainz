//! Persistent per-endpoint health history (`<state_dir>/health.json`).

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::capabilities::Capabilities;
use crate::error::ErrorClass;
use crate::probe::ProbeResult;

const FILE_NAME: &str = "health.json";
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
            at_ms: now_ms(),
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
    dir: PathBuf,
    entries: BTreeMap<Key, Entry>,
}

impl HealthStore {
    /// Samples kept per endpoint.
    pub const CAPACITY: usize = 64;

    /// Load the store from `state_dir`. A missing or corrupt file yields an
    /// empty store; other I/O errors (e.g. permissions) are returned.
    pub fn load(state_dir: &Path) -> io::Result<Self> {
        Ok(HealthStore {
            dir: state_dir.to_path_buf(),
            entries: read_entries(&state_dir.join(FILE_NAME))?,
        })
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

    /// Median latency over the last `window` successful samples (lower
    /// median for even counts); `None` when there are none.
    pub fn p50(&self, chain_id: u64, endpoint: &str, window: usize) -> Option<Duration> {
        let entry = self.entry(chain_id, endpoint)?;
        let mut lat: Vec<u32> = entry
            .samples
            .iter()
            .rev()
            .filter(|s| s.error.is_none())
            .take(window)
            .map(|s| s.latency_ms)
            .collect();
        if lat.is_empty() {
            return None;
        }
        lat.sort_unstable();
        Some(Duration::from_millis(u64::from(lat[(lat.len() - 1) / 2])))
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

    /// Under an exclusive lock file `<state_dir>/health.lock`, re-read the
    /// file, merge this store's samples and capabilities into it, and write
    /// it atomically (tempfile + fsync + rename, mode 0600, dir 0700).
    ///
    /// Merging is idempotent: samples are de-duplicated, ordered by time and
    /// trimmed to [`Self::CAPACITY`]; for capabilities the most recently
    /// probed wins.
    pub fn save(&self) -> io::Result<()> {
        create_private_dir(&self.dir)?;
        let lock = open_private(&self.dir.join(LOCK_NAME))?;
        lock.lock()?;
        let path = self.dir.join(FILE_NAME);
        let mut merged = read_entries(&path)?;
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
        let result = write_atomic(&self.dir, &path, &json);
        let _ = lock.unlock();
        result
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

fn read_entries(path: &Path) -> io::Result<BTreeMap<Key, Entry>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(e),
    };
    let Ok(file) = serde_json::from_slice::<FileFormat>(&bytes) else {
        return Ok(BTreeMap::new());
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
    Ok(entries)
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

fn open_private(path: &Path) -> io::Result<fs::File> {
    let mut opts = fs::OpenOptions::new();
    opts.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

fn write_atomic(dir: &Path, path: &Path, data: &[u8]) -> io::Result<()> {
    // NamedTempFile is created with mode 0600 on unix.
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(data)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    #[cfg(unix)]
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
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
    fn p50_over_successful_window() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = HealthStore::load(dir.path()).unwrap();
        assert_eq!(store.p50(1, "a", 10), None);
        for (i, ms) in [100, 10, 20, 30].into_iter().enumerate() {
            store.record(1, "a", sample(i as u64, ms, true));
        }
        store.record(1, "a", sample(9, 1, false)); // failures ignored
        assert_eq!(store.p50(1, "a", 3), Some(Duration::from_millis(20)));
        // window 4 => [10, 20, 30, 100], lower median 20
        assert_eq!(store.p50(1, "a", 4), Some(Duration::from_millis(20)));
        assert_eq!(store.p50(1, "a", 1), Some(Duration::from_millis(30)));
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

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir.join(FILE_NAME)), 0o600);
            assert_eq!(mode(&dir), 0o700);
        }
    }

    #[test]
    fn corrupt_file_is_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(FILE_NAME), b"{not json").unwrap();
        let mut store = HealthStore::load(dir.path()).unwrap();
        assert!(store.samples(1, "a").is_empty());
        store.record(1, "a", sample(1, 5, true));
        store.save().unwrap();
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
