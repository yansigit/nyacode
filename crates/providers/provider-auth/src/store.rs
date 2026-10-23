use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use codex_provider_api::ProviderError;
use codex_provider_api::Result;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::Digest;
use sha2::Sha256;
use tokio_util::sync::CancellationToken;

fn io_error(_: impl std::fmt::Display) -> ProviderError {
    ProviderError::Checkpoint("unable to read or persist private provider state".into())
}

pub fn digest(value: &[u8]) -> String {
    hex::encode(Sha256::digest(value))
}

/// Files are addressed by hashes of structured keys, never by remote path input.
#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(codex_home: &Path) -> Self {
        Self {
            root: codex_home.join("native-providers"),
        }
    }

    fn directory(&self) -> Result<()> {
        if let Ok(meta) = fs::symlink_metadata(&self.root)
            && (meta.file_type().is_symlink() || !meta.is_dir())
        {
            return Err(io_error("unsafe directory"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder.create(&self.root).map_err(io_error)?;
        }
        #[cfg(not(unix))]
        fs::create_dir_all(&self.root).map_err(io_error)?;
        Ok(())
    }

    fn path(&self, key: &str, extension: &str) -> PathBuf {
        self.root
            .join(format!("{}.{}", digest(key.as_bytes()), extension))
    }

    /// Recovery probes never wait behind another process's inference request.
    pub fn try_lock(&self, key: &str) -> Result<Option<StoreLock>> {
        self.directory()?;
        let path = self.path(key, "lock");
        if fs::symlink_metadata(&path).is_ok_and(|meta| !meta.is_file()) {
            return Err(io_error("unsafe lock file"));
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(path).map_err(io_error)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(StoreLock {
                file,
                store: self.clone(),
                key: key.into(),
            })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(_) => Err(io_error("lock failed")),
        }
    }

    pub async fn lock(&self, key: &str, cancellation: &CancellationToken) -> Result<StoreLock> {
        self.directory()?;
        let path = self.path(key, "lock");
        if fs::symlink_metadata(&path).is_ok_and(|meta| !meta.is_file()) {
            return Err(io_error("unsafe lock file"));
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(path).map_err(io_error)?;
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if cancellation.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            match file.try_lock() {
                Ok(()) => {
                    return Ok(StoreLock {
                        file,
                        store: self.clone(),
                        key: key.to_owned(),
                    });
                }
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    tokio::select! {
                        _ = cancellation.cancelled() => return Err(ProviderError::Cancelled),
                        _ = tokio::time::sleep(Duration::from_millis(20)) => {}
                    }
                }
                Err(_) => return Err(io_error("lock failed")),
            }
        }
    }

    pub fn read<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        if fs::symlink_metadata(&self.root)
            .is_ok_and(|meta| meta.file_type().is_symlink() || !meta.is_dir())
        {
            return Err(io_error("unsafe state directory"));
        }
        let path = self.path(key, "json");
        if fs::symlink_metadata(&path).is_ok_and(|meta| !meta.is_file()) {
            return Err(io_error("unsafe state file"));
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        match options.open(path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(64 * 1024 * 1024 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(io_error)?;
                if bytes.len() > 64 * 1024 * 1024 {
                    return Err(io_error("state exceeds size limit"));
                }
                serde_json::from_slice(&bytes).map(Some).map_err(io_error)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io_error(error)),
        }
    }
}

/// The stable lock file is never removed, including during logout.
pub struct StoreLock {
    file: File,
    store: Store,
    key: String,
}

impl StoreLock {
    pub fn read<T: DeserializeOwned>(&self) -> Result<Option<T>> {
        self.store.read(&self.key)
    }
    pub fn write<T: Serialize>(&self, value: &T) -> Result<()> {
        let mut temp = tempfile::NamedTempFile::new_in(&self.store.root).map_err(io_error)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temp.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(io_error)?;
        }
        serde_json::to_writer(temp.as_file_mut(), value).map_err(io_error)?;
        if temp.as_file().metadata().map_err(io_error)?.len() > 64 * 1024 * 1024 {
            return Err(io_error("state exceeds size limit"));
        }
        temp.as_file_mut().flush().map_err(io_error)?;
        temp.as_file().sync_all().map_err(io_error)?;
        temp.persist(self.store.path(&self.key, "json"))
            .map_err(io_error)?;
        #[cfg(unix)]
        File::open(&self.store.root)
            .and_then(|dir| dir.sync_all())
            .map_err(io_error)?;
        Ok(())
    }
    pub fn delete(&self) -> Result<()> {
        match fs::remove_file(self.store.path(&self.key, "json")) {
            Ok(()) => {
                #[cfg(unix)]
                File::open(&self.store.root)
                    .and_then(|dir| dir.sync_all())
                    .map_err(io_error)?;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error(error)),
        }
    }
}
impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}
