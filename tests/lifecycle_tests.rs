use serial_test::serial;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tacky_borders::config::ConfigWatcher;

static CONFIG_CALLBACKS: AtomicUsize = AtomicUsize::new(0);

fn count_config_callback() {
    CONFIG_CALLBACKS.fetch_add(1, Ordering::AcqRel);
}

fn noop_config_callback() {}

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> anyhow::Result<Self> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!("tb-{label}-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }

    fn path(&self) -> &PathBuf {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
#[serial]
fn test_config_watcher_pending_read_shutdown() -> anyhow::Result<()> {
    let dir = TempDir::new("config-watcher-drop")?;
    let config_path = dir.path().join("config.yaml");
    fs::write(&config_path, "watch_config_changes: true\n")?;

    // Drop immediately after creation to repeatedly exercise every startup-vs-stop interleaving,
    // including a pending overlapped directory read.
    for _ in 0..100 {
        let watcher = ConfigWatcher::new(config_path.clone(), 25, noop_config_callback)?;
        drop(watcher);
    }

    Ok(())
}

#[test]
#[serial]
fn test_config_watcher_atomic_replace_notifies() -> anyhow::Result<()> {
    CONFIG_CALLBACKS.store(0, Ordering::Release);
    let dir = TempDir::new("config-watcher-replace")?;
    let config_path = dir.path().join("config.yaml");
    fs::write(&config_path, "watch_config_changes: true\n")?;

    let watcher = ConfigWatcher::new(config_path.clone(), 25, count_config_callback)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut attempt = 0usize;

    while CONFIG_CALLBACKS.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
        let replacement = dir.path().join(format!("config.yaml.tmp-{attempt}"));
        fs::write(&replacement, format!("# replacement {attempt}\n"))?;
        if config_path.exists() {
            fs::remove_file(&config_path)?;
        }
        fs::rename(&replacement, &config_path)?;
        attempt += 1;
        thread::sleep(Duration::from_millis(10));
    }

    drop(watcher);
    assert!(
        CONFIG_CALLBACKS.load(Ordering::Acquire) > 0,
        "config watcher did not observe an atomic-style file replacement"
    );
    Ok(())
}
