//! Host-owned stop receipts, independent of any connected client.

use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

pub(crate) const START_EPOCH_ENV: &str = "HERDR_START_EPOCH";

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
pub(crate) struct Receipt {
    pub(crate) epoch: u64,
    pub(crate) stopped: bool,
}

pub(crate) struct Guard {
    _lock: File,
    path: PathBuf,
    pub(crate) receipt: Receipt,
}

pub(super) fn receipt_path(api_path: &Path) -> io::Result<PathBuf> {
    sidecar_path(api_path, ".lifecycle.json")
}

fn sidecar_path(api_path: &Path, suffix: &str) -> io::Result<PathBuf> {
    let identity = crate::platform::server_lifecycle_identity(api_path)?;
    let key = format!("{:x}", Sha256::digest(identity));
    let absolute = std::path::absolute(api_path)?;
    let parent = absolute.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "invalid server endpoint path")
    })?;
    // Named-session deletion must not erase the fence against its delayed children.
    let root = parent
        .parent()
        .filter(|directory| {
            directory
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("sessions"))
        })
        .unwrap_or(parent);
    Ok(root.join(format!(".herdr-{key}{suffix}")))
}

pub(crate) fn read(api_path: &Path) -> io::Result<Receipt> {
    match std::fs::read(receipt_path(api_path)?) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Receipt::default()),
        Err(error) => Err(error),
    }
}

pub(crate) fn lock(api_path: &Path) -> io::Result<Guard> {
    let lock_path = sidecar_path(api_path, ".lifecycle.lock")?;
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match crate::platform::create_private_state_file(&lock_path) {
        Ok(file) => drop(file),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let file = OpenOptions::new().read(true).write(true).open(lock_path)?;
    file.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => io::Error::new(
            io::ErrorKind::WouldBlock,
            "server is busy; retry after its current operation finishes",
        ),
        std::fs::TryLockError::Error(error) => error,
    })?;
    Ok(Guard {
        _lock: file,
        path: receipt_path(api_path)?,
        receipt: read(api_path)?,
    })
}

/// Status probes briefly hold this lock too; startup may wait, then recheck its fence.
pub(crate) fn lock_for_start(api_path: &Path) -> io::Result<Guard> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match lock(api_path) {
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            result => return result,
        }
    }
}

#[cfg(test)]
pub(crate) fn remove_test_receipt(api_path: &Path) {
    for suffix in [".lifecycle.json", ".lifecycle.lock"] {
        std::fs::remove_file(sidecar_path(api_path, suffix).unwrap()).unwrap();
    }
}

impl Guard {
    pub(crate) fn confirm_durable(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            crate::platform::sync_parent_directory(parent)?;
        }
        Ok(())
    }

    pub(crate) fn require_epoch(&self, expected: u64) -> io::Result<()> {
        if self.receipt.epoch != expected {
            return Err(io::Error::other(
                "server state changed; refresh before starting or stopping",
            ));
        }
        Ok(())
    }

    pub(crate) fn next_epoch(&self) -> io::Result<u64> {
        self.receipt
            .epoch
            .checked_add(1)
            .ok_or_else(|| io::Error::other("server lifecycle epoch exhausted"))
    }

    pub(crate) fn commit(&mut self, stopped: bool) -> io::Result<u64> {
        let receipt = Receipt {
            epoch: self.next_epoch()?,
            stopped,
        };
        let temporary = self.path.with_extension("json.tmp");
        // The stable lock serializes writers, including cleanup after a failed write.
        match std::fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let result = (|| {
            let mut file = crate::platform::create_private_state_file(&temporary)?;
            file.write_all(&serde_json::to_vec(&receipt)?)?;
            file.sync_all()?;
            drop(file);
            crate::platform::replace_file(&temporary, &self.path)?;
            self.receipt = receipt;
            self.confirm_durable()?;
            Ok(receipt.epoch)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temporary);
        }
        result
    }
}

pub(crate) fn launch_epoch() -> io::Result<u64> {
    Ok(lock(&crate::api::socket_path())?.receipt.epoch)
}

pub(crate) fn take_expected_epoch() -> io::Result<Option<u64>> {
    let value = std::env::var_os(START_EPOCH_ENV);
    std::env::remove_var(START_EPOCH_ENV);
    value
        .map(|value| {
            value.to_string_lossy().parse::<u64>().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid server startup epoch")
            })
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_aliases_share_the_lifecycle_lock() {
        let base = std::env::temp_dir().join(format!(
            "herdr-life-alias-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for path in [
            base.join("server.sock"),
            base.join("sessions/named/server.sock"),
        ] {
            #[cfg(windows)]
            let alias = PathBuf::from(path.to_string_lossy().to_uppercase());
            #[cfg(not(windows))]
            let alias = path
                .parent()
                .unwrap()
                .join(".")
                .join(path.file_name().unwrap());
            let mut guard = lock(&path).unwrap();
            guard.commit(false).unwrap();
            assert_eq!(
                lock(&alias).err().unwrap().kind(),
                io::ErrorKind::WouldBlock
            );
            let waiter = std::thread::spawn({
                let alias = alias.clone();
                move || lock_for_start(&alias).unwrap()
            });
            guard.commit(true).unwrap();
            drop(guard);
            let admitted = waiter.join().unwrap();
            assert!(admitted.receipt.stopped);
            assert!(admitted.require_epoch(1).is_err());
            drop(admitted);
            let starts = std::thread::scope(|scope| {
                let attempt = || {
                    let mut guard = lock_for_start(&path).unwrap();
                    guard.require_epoch(2).and_then(|_| guard.commit(false))
                };
                let first = scope.spawn(attempt);
                let second = scope.spawn(attempt);
                [first.join().unwrap(), second.join().unwrap()]
            });
            assert_eq!(starts.iter().filter(|result| result.is_ok()).count(), 1);
            remove_test_receipt(&path);
        }
        std::fs::remove_dir_all(base).unwrap();
    }
}
