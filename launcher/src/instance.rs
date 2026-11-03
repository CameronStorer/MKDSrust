//! One launcher per data folder. The first launcher holds an exclusive lock on `instance.lock`; a second one (for
//! example started by Windows for a `mkdsrust://` link) drops its link into `inbox/` and exits, and the running
//! launcher picks it up. The OS releases the lock when a launcher exits or crashes, so it can never go stale.
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    time::Duration,
};

const LOCK: &str = "instance.lock";
const INBOX: &str = "inbox";

pub enum Role {
    Primary(Primary),
    /// Another launcher already runs with this data folder.
    Secondary,
}
pub struct Primary {
    _lock: File,
    inbox: PathBuf,
}

pub fn acquire(dir: &Path) -> std::io::Result<Role> {
    std::fs::create_dir_all(dir)?;
    let file = File::options().create(true).truncate(false).read(true).write(true).open(dir.join(LOCK))?;
    match file.try_lock() {
        Ok(()) => {
            let inbox = dir.join(INBOX);
            // messages left behind by a launcher that already exited are not acted on
            let _ = std::fs::remove_dir_all(&inbox);
            Ok(Role::Primary(Primary { _lock: file, inbox }))
        }
        Err(std::fs::TryLockError::WouldBlock) => Ok(Role::Secondary),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

/// Hand `message` (a `mkdsrust:` link, or empty to just bring the window forward) to the running launcher.
pub fn send(dir: &Path, message: &str) -> std::io::Result<()> {
    let inbox = dir.join(INBOX);
    std::fs::create_dir_all(&inbox)?;
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    let name = format!("{nanos:032}-{}", std::process::id());
    let tmp = inbox.join(format!("{name}.tmp"));
    std::fs::write(&tmp, message)?;
    // the rename makes the message appear complete
    std::fs::rename(&tmp, inbox.join(format!("{name}.msg")))
}

impl Primary {
    /// Messages that arrived, oldest first. Each is removed once read.
    pub fn take(&self) -> Vec<String> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&self.inbox)
            .map(|d| d.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "msg")).collect())
            .unwrap_or_default();
        files.sort();
        files
            .into_iter()
            .filter_map(|p| {
                let text = std::fs::read_to_string(&p).ok();
                let _ = std::fs::remove_file(&p);
                text.map(|t| t.chars().take(8192).collect())
            })
            .collect()
    }
    /// Watch the inbox on a background thread (also while the window is minimized); `wake` runs after each message.
    pub fn watch(self, wake: impl Fn() + Send + 'static) -> Receiver<String> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            loop {
                for m in self.take() {
                    if tx.send(m).is_err() {
                        return;
                    }
                    wake();
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        });
        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn second_launcher_hands_its_link_to_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let Role::Primary(first) = acquire(dir.path()).unwrap() else { panic!("first launcher must be primary") };
        assert!(matches!(acquire(dir.path()).unwrap(), Role::Secondary));
        assert!(first.take().is_empty());
        send(dir.path(), "mkdsrust://install?url=https%3A%2F%2Fmkdsrust.com%2Fx").unwrap();
        send(dir.path(), "").unwrap();
        // a half-written message is not picked up
        std::fs::write(dir.path().join("inbox/zzz.tmp"), "partial").unwrap();
        assert_eq!(first.take(), vec!["mkdsrust://install?url=https%3A%2F%2Fmkdsrust.com%2Fx".to_string(), String::new()]);
        assert!(first.take().is_empty());

        let rx = first.watch(|| {});
        send(dir.path(), "mkdsrust://install?url=b").unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), "mkdsrust://install?url=b");
        drop(rx);
    }

    #[test]
    fn lock_is_released_with_the_launcher_and_stale_messages_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let Role::Primary(first) = acquire(dir.path()).unwrap() else { panic!() };
        send(dir.path(), "old").unwrap();
        drop(first);
        let Role::Primary(second) = acquire(dir.path()).unwrap() else { panic!("lock must be free after exit") };
        assert!(second.take().is_empty());
    }
}
