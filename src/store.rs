use crate::envfile;
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
use thiserror::Error;
use tokio::sync::RwLock;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotFound(String),
    #[error("key `{0}` already exists")]
    Conflict(String),
    #[error("invalid env file:\n{0}")]
    Validation(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone)]
pub struct StorePaths {
    pub root: PathBuf,
}

impl StorePaths {
    pub fn key_dir(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    pub fn env_file(&self, name: &str) -> PathBuf {
        self.key_dir(name).join("secrets.env")
    }

    pub fn files_dir(&self, name: &str) -> PathBuf {
        self.key_dir(name).join("files")
    }
}

#[derive(Debug, Clone)]
pub struct StoredFile {
    pub name: String,
    pub bytes: u64,
}

/// Owns local-secrets persistence and hides all filesystem safety rules from
/// HTTP handlers: private permissions, atomic writes, path validation, and the
/// env-file grammar are enforced here.
///
/// Storage is organised as a flat namespace of "keys" under the root
/// directory. Each key is a subdirectory containing its own env file and
/// shared-files directory, so multiple unrelated services can keep their
/// settings apart.
#[derive(Debug, Clone)]
pub struct Store {
    paths: StorePaths,
    lock: Arc<RwLock<()>>,
}

impl Store {
    pub fn open(root: PathBuf) -> Result<Self, StoreError> {
        ensure_private_dir(&root)?;

        let store = Self {
            paths: StorePaths { root },
            lock: Arc::new(RwLock::new(())),
        };

        // Make sure the user always has at least one key to land on.
        if store.list_keys_sync()?.is_empty() {
            store.create_key_sync(DEFAULT_KEY)?;
        }

        Ok(store)
    }

    pub fn paths(&self) -> &StorePaths {
        &self.paths
    }

    // ----- keys ------------------------------------------------------------

    pub async fn list_keys(&self) -> Result<Vec<String>, StoreError> {
        let _guard = self.lock.read().await;
        self.list_keys_sync()
    }

    fn list_keys_sync(&self) -> Result<Vec<String>, StoreError> {
        let mut keys = Vec::new();

        for entry in fs::read_dir(&self.paths.root)? {
            let entry = entry?;
            let file_type = entry.file_type()?;

            if !file_type.is_dir() {
                continue;
            }

            let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
                continue;
            };

            if !is_valid_key_name(&name) {
                continue;
            }

            // A key directory must contain its env file. Stray subdirectories
            // left over from older layouts are ignored.
            if !self.paths.env_file(&name).is_file() {
                continue;
            }

            keys.push(name);
        }

        keys.sort();
        Ok(keys)
    }

    pub async fn create_key(&self, name: &str) -> Result<(), StoreError> {
        validate_key_name(name)?;
        let _guard = self.lock.write().await;
        self.create_key_sync(name)
    }

    fn create_key_sync(&self, name: &str) -> Result<(), StoreError> {
        let dir = self.paths.key_dir(name);

        if dir.exists() {
            return Err(StoreError::Conflict(name.to_string()));
        }

        ensure_private_dir(&dir)?;
        ensure_private_dir(&self.paths.files_dir(name))?;
        ensure_private_file(&self.paths.env_file(name))?;
        Ok(())
    }

    pub async fn rename_key(&self, old: &str, new: &str) -> Result<(), StoreError> {
        validate_key_name(old)?;
        validate_key_name(new)?;
        let _guard = self.lock.write().await;

        let old_dir = self.paths.key_dir(old);
        let new_dir = self.paths.key_dir(new);

        if !old_dir.is_dir() {
            return Err(StoreError::NotFound(format!("key `{old}` does not exist")));
        }

        if new_dir.exists() {
            return Err(StoreError::Conflict(new.to_string()));
        }

        fs::rename(&old_dir, &new_dir)?;
        Ok(())
    }

    pub async fn delete_key(&self, name: &str) -> Result<(), StoreError> {
        validate_key_name(name)?;
        let _guard = self.lock.write().await;

        let dir = self.paths.key_dir(name);

        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(StoreError::Io(error)),
        }
    }

    // ----- env -------------------------------------------------------------

    pub async fn read_env_file(&self, key: &str) -> Result<String, StoreError> {
        validate_key_name(key)?;
        let _guard = self.lock.read().await;

        match fs::read_to_string(self.paths.env_file(key)) {
            Ok(contents) => Ok(contents),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(StoreError::Io(error)),
        }
    }

    pub async fn read_env_map(&self, key: &str) -> Result<BTreeMap<String, String>, StoreError> {
        let contents = self.read_env_file(key).await?;
        parse_env_for_storage(&contents)
    }

    pub async fn write_env_file(&self, key: &str, contents: &str) -> Result<(), StoreError> {
        validate_key_name(key)?;
        parse_env_for_storage(contents)?;

        let _guard = self.lock.write().await;
        ensure_private_dir(&self.paths.key_dir(key))?;
        ensure_private_dir(&self.paths.files_dir(key))?;
        atomic_write_private(&self.paths.env_file(key), contents.as_bytes())
    }

    // ----- shared files ----------------------------------------------------

    pub async fn list_files(&self, key: &str) -> Result<Vec<StoredFile>, StoreError> {
        validate_key_name(key)?;
        let _guard = self.lock.read().await;

        let files_dir = self.paths.files_dir(key);

        if !files_dir.is_dir() {
            return Ok(Vec::new());
        }

        let mut files = Vec::new();

        for entry in fs::read_dir(&files_dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;

            if !file_type.is_file() {
                continue;
            }

            let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
                continue;
            };

            if !is_valid_file_name(&name) {
                continue;
            }

            files.push(StoredFile {
                name,
                bytes: entry.metadata()?.len(),
            });
        }

        files.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(files)
    }

    pub async fn read_named_file(&self, key: &str, name: &str) -> Result<Vec<u8>, StoreError> {
        validate_key_name(key)?;
        let path = self.file_path(key, name)?;
        let _guard = self.lock.read().await;

        match fs::read(&path) {
            Ok(bytes) => Ok(bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Err(StoreError::NotFound(
                format!("file `{name}` does not exist in key `{key}`"),
            )),
            Err(error) => Err(StoreError::Io(error)),
        }
    }

    pub async fn write_named_file(
        &self,
        key: &str,
        name: &str,
        bytes: &[u8],
    ) -> Result<(), StoreError> {
        validate_key_name(key)?;
        let path = self.file_path(key, name)?;
        let _guard = self.lock.write().await;

        ensure_private_dir(&self.paths.key_dir(key))?;
        ensure_private_dir(&self.paths.files_dir(key))?;
        atomic_write_private(&path, bytes)
    }

    pub async fn delete_named_file(&self, key: &str, name: &str) -> Result<(), StoreError> {
        validate_key_name(key)?;
        let path = self.file_path(key, name)?;
        let _guard = self.lock.write().await;

        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(StoreError::Io(error)),
        }
    }

    fn file_path(&self, key: &str, name: &str) -> Result<PathBuf, StoreError> {
        validate_file_name(name)?;
        Ok(self.paths.files_dir(key).join(name))
    }
}

pub const DEFAULT_KEY: &str = "default";

fn parse_env_for_storage(contents: &str) -> Result<BTreeMap<String, String>, StoreError> {
    envfile::parse(contents).map_err(|errors| StoreError::Validation(errors.join("\n")))
}

pub fn is_valid_key_name(name: &str) -> bool {
    validate_key_name(name).is_ok()
}

pub fn validate_key_name(name: &str) -> Result<(), StoreError> {
    validate_segment_name(name, "key")
}

pub fn is_valid_file_name(name: &str) -> bool {
    validate_file_name(name).is_ok()
}

pub fn validate_file_name(name: &str) -> Result<(), StoreError> {
    validate_segment_name(name, "file")
}

fn validate_segment_name(name: &str, kind: &str) -> Result<(), StoreError> {
    if name.is_empty() || name == "." || name == ".." {
        return Err(StoreError::BadRequest(format!(
            "{kind} names must not be empty, `.` or `..`"
        )));
    }

    if name.len() > 255 {
        return Err(StoreError::BadRequest(format!(
            "{kind} names must be 255 bytes or fewer"
        )));
    }

    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        return Err(StoreError::BadRequest(format!(
            "`{name}` is not a valid {kind} name; use only letters, numbers, dots, underscores, and dashes"
        )));
    }

    Ok(())
}

fn ensure_private_dir(path: &Path) -> Result<(), StoreError> {
    fs::create_dir_all(path)?;
    reject_symlink(path)?;

    if !path.is_dir() {
        return Err(StoreError::BadRequest(format!(
            "`{}` exists but is not a directory",
            path.display()
        )));
    }

    set_permissions(path, 0o700)
}

fn ensure_private_file(path: &Path) -> Result<(), StoreError> {
    if path.exists() {
        reject_symlink(path)?;

        if !path.is_file() {
            return Err(StoreError::BadRequest(format!(
                "`{}` exists but is not a regular file",
                path.display()
            )));
        }

        return set_permissions(path, 0o600);
    }

    create_private_file(path).map(|_| ())
}

fn reject_symlink(path: &Path) -> Result<(), StoreError> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(StoreError::BadRequest(format!(
            "refusing to use symlink `{}` for secret storage",
            path.display()
        )));
    }

    Ok(())
}

fn create_private_file(path: &Path) -> Result<File, StoreError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);

    #[cfg(unix)]
    options.mode(0o600);

    let file = options.open(path)?;
    set_permissions(path, 0o600)?;
    Ok(file)
}

fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let parent = path.parent().ok_or_else(|| {
        StoreError::BadRequest(format!("`{}` has no parent directory", path.display()))
    })?;

    ensure_private_dir(parent)?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| StoreError::BadRequest(format!("invalid path `{}`", path.display())))?;
    let temp_path = parent.join(format!(
        ".{file_name}.tmp.{}.{}",
        std::process::id(),
        unique_suffix()
    ));

    let mut file = create_private_file(&temp_path)?;

    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temp_path);
        return Err(StoreError::Io(error));
    }

    drop(file);
    fs::rename(&temp_path, path).inspect_err(|_| {
        let _ = fs::remove_file(&temp_path);
    })?;
    set_permissions(path, 0o600)
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

#[cfg(unix)]
fn set_permissions(path: &Path, mode: u32) -> Result<(), StoreError> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(StoreError::from)
}

#[cfg(not(unix))]
fn set_permissions(_path: &Path, _mode: u32) -> Result<(), StoreError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_safe_file_names() {
        assert!(is_valid_file_name(".npmrc"));
        assert!(is_valid_file_name("service-account.json"));
        assert!(!is_valid_file_name("../secret"));
        assert!(!is_valid_file_name("nested/file"));
        assert!(!is_valid_file_name(""));
    }

    #[test]
    fn validates_safe_key_names() {
        assert!(is_valid_key_name("default"));
        assert!(is_valid_key_name("my-service"));
        assert!(is_valid_key_name("v2.alpha"));
        assert!(!is_valid_key_name("../escape"));
        assert!(!is_valid_key_name("nested/key"));
        assert!(!is_valid_key_name(""));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn creates_storage_with_private_permissions() {
        let root = std::env::temp_dir().join(format!(
            "local-secrets-store-test-{}-{}",
            std::process::id(),
            unique_suffix()
        ));

        let store = Store::open(root.clone()).unwrap();

        assert_mode(&root, 0o700);
        assert_mode(&store.paths().key_dir(DEFAULT_KEY), 0o700);
        assert_mode(&store.paths().files_dir(DEFAULT_KEY), 0o700);
        assert_mode(&store.paths().env_file(DEFAULT_KEY), 0o600);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    fn assert_mode(path: &Path, expected: u32) {
        let actual = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(actual, expected, "unexpected mode for {}", path.display());
    }
}
