//! Where pi's sessions are kept: the seven whole-file calls `js/src/fs.js`
//! makes, the same contract as Dart's `PiSessionStore`.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use super::BoxFuture;

/// A file in a [`SessionStore`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredFile {
    /// Absolute, POSIX: `/sessions/.../<id>.jsonl`.
    pub path: String,
    /// Only compared, never summed.
    pub size: u64,
    pub mtime_ms: u64,
}

impl StoredFile {
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "path": self.path, "size": self.size, "mtimeMs": self.mtime_ms })
    }
}

pub type StoreResult<T> = Result<T, String>;

/// Where pi's sessions are kept: whole text files by absolute POSIX path.
/// Directories are not stored; one exists while a file under it does.
pub trait SessionStore: Send + Sync {
    /// The file's text, `None` when there is none. With `max_lines`, at least
    /// that many leading lines when the file has them.
    fn read(&self, path: &str, max_lines: Option<usize>) -> BoxFuture<'_, StoreResult<Option<String>>>;
    fn write(&self, path: &str, text: String) -> BoxFuture<'_, StoreResult<()>>;
    /// Creates the file when it does not exist.
    fn append(&self, path: &str, text: String) -> BoxFuture<'_, StoreResult<()>>;
    fn rename(&self, from: &str, to: &str) -> BoxFuture<'_, StoreResult<()>>;
    /// A file, or with `recursive` every file under the directory `path`.
    fn remove(&self, path: &str, recursive: bool) -> BoxFuture<'_, StoreResult<()>>;
    fn stat(&self, path: &str) -> BoxFuture<'_, StoreResult<Option<StoredFile>>>;
    /// Every file under `dir`, at any depth.
    fn list(&self, dir: &str) -> BoxFuture<'_, StoreResult<Vec<StoredFile>>>;
}

/// The relative path under a root that `path` names, or an error when it
/// would leave the root. pi only writes paths it built, but the store does
/// not take that on trust.
fn relative(path: &str) -> StoreResult<PathBuf> {
    let mut out = PathBuf::new();
    for c in Path::new(path).components() {
        match c {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(p) => out.push(p),
            Component::ParentDir | Component::Prefix(_) => return Err(format!("path leaves the store: {path}")),
        }
    }
    Ok(out)
}

fn ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Sessions as files under a directory, readable by the owner only.
pub struct DirectoryStore {
    root: PathBuf,
}

impl DirectoryStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn file(&self, path: &str) -> StoreResult<PathBuf> {
        Ok(self.root.join(relative(path)?))
    }

    async fn parent(&self, file: &Path) -> StoreResult<()> {
        let Some(dir) = file.parent() else { return Ok(()) };
        let mut b = tokio::fs::DirBuilder::new();
        b.recursive(true);
        #[cfg(unix)]
        b.mode(0o700);
        b.create(dir).await.map_err(|e| e.to_string())
    }

    async fn open(file: &Path, append: bool) -> std::io::Result<tokio::fs::File> {
        let mut o = tokio::fs::OpenOptions::new();
        o.create(true);
        if append {
            o.append(true);
        } else {
            o.write(true).truncate(true);
        }
        #[cfg(unix)]
        o.mode(0o600);
        o.open(file).await
    }

    async fn put(&self, path: &str, text: String, append: bool) -> StoreResult<()> {
        use tokio::io::AsyncWriteExt;
        let file = self.file(path)?;
        self.parent(&file).await?;
        let mut f = Self::open(&file, append).await.map_err(|e| e.to_string())?;
        f.write_all(text.as_bytes()).await.map_err(|e| e.to_string())?;
        f.sync_data().await.map_err(|e| e.to_string())
    }
}

impl SessionStore for DirectoryStore {
    fn read(&self, path: &str, _max_lines: Option<usize>) -> BoxFuture<'_, StoreResult<Option<String>>> {
        let file = self.file(path);
        Box::pin(async move {
            match tokio::fs::read_to_string(file?).await {
                Ok(s) => Ok(Some(s)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.to_string()),
            }
        })
    }

    fn write(&self, path: &str, text: String) -> BoxFuture<'_, StoreResult<()>> {
        let path = path.to_string();
        Box::pin(async move { self.put(&path, text, false).await })
    }

    fn append(&self, path: &str, text: String) -> BoxFuture<'_, StoreResult<()>> {
        let path = path.to_string();
        Box::pin(async move { self.put(&path, text, true).await })
    }

    fn rename(&self, from: &str, to: &str) -> BoxFuture<'_, StoreResult<()>> {
        let (from, to) = (self.file(from), self.file(to));
        Box::pin(async move {
            let to = to?;
            self.parent(&to).await?;
            tokio::fs::rename(from?, to).await.map_err(|e| e.to_string())
        })
    }

    fn remove(&self, path: &str, recursive: bool) -> BoxFuture<'_, StoreResult<()>> {
        let file = self.file(path);
        Box::pin(async move {
            let file = file?;
            let meta = match tokio::fs::symlink_metadata(&file).await {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e.to_string()),
            };
            let r = if meta.is_dir() {
                if recursive { tokio::fs::remove_dir_all(&file).await } else { tokio::fs::remove_dir(&file).await }
            } else {
                tokio::fs::remove_file(&file).await
            };
            r.map_err(|e| e.to_string())
        })
    }

    fn stat(&self, path: &str) -> BoxFuture<'_, StoreResult<Option<StoredFile>>> {
        let file = self.file(path);
        let path = path.to_string();
        Box::pin(async move {
            match tokio::fs::metadata(file?).await {
                Ok(m) if m.is_file() => Ok(Some(StoredFile {
                    path,
                    size: m.len(),
                    mtime_ms: m.modified().map(ms).unwrap_or(0),
                })),
                Ok(_) => Ok(None),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.to_string()),
            }
        })
    }

    fn list(&self, dir: &str) -> BoxFuture<'_, StoreResult<Vec<StoredFile>>> {
        let start = self.file(dir);
        let prefix = format!("/{}", relative(dir).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default());
        Box::pin(async move {
            let start = start?;
            let mut out = Vec::new();
            let mut todo = vec![(start, prefix.trim_end_matches('/').to_string())];
            while let Some((dir, at)) = todo.pop() {
                let mut entries = match tokio::fs::read_dir(&dir).await {
                    Ok(e) => e,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e.to_string()),
                };
                while let Some(e) = entries.next_entry().await.map_err(|e| e.to_string())? {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let path = format!("{at}/{name}");
                    // Never followed: a link is not something pi wrote.
                    let t = e.file_type().await.map_err(|e| e.to_string())?;
                    if t.is_dir() {
                        todo.push((e.path(), path));
                    } else if t.is_file() {
                        let m = e.metadata().await.map_err(|e| e.to_string())?;
                        out.push(StoredFile { path, size: m.len(), mtime_ms: m.modified().map(ms).unwrap_or(0) });
                    }
                }
            }
            Ok(out)
        })
    }
}

/// Sessions in memory: for tests, and for conversations nobody keeps.
#[derive(Default)]
pub struct MemoryStore {
    files: Mutex<BTreeMap<String, (String, u64)>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn now() -> u64 {
        ms(SystemTime::now())
    }

    fn key(path: &str) -> StoreResult<String> {
        Ok(format!("/{}", relative(path)?.to_string_lossy().replace('\\', "/")))
    }
}

impl SessionStore for MemoryStore {
    fn read(&self, path: &str, _max_lines: Option<usize>) -> BoxFuture<'_, StoreResult<Option<String>>> {
        let r = Self::key(path).map(|k| self.files.lock().unwrap().get(&k).map(|f| f.0.clone()));
        Box::pin(async move { r })
    }

    fn write(&self, path: &str, text: String) -> BoxFuture<'_, StoreResult<()>> {
        let r = Self::key(path).map(|k| {
            self.files.lock().unwrap().insert(k, (text, Self::now()));
        });
        Box::pin(async move { r })
    }

    fn append(&self, path: &str, text: String) -> BoxFuture<'_, StoreResult<()>> {
        let r = Self::key(path).map(|k| {
            let mut files = self.files.lock().unwrap();
            let f = files.entry(k).or_default();
            f.0.push_str(&text);
            f.1 = Self::now();
        });
        Box::pin(async move { r })
    }

    fn rename(&self, from: &str, to: &str) -> BoxFuture<'_, StoreResult<()>> {
        let r = (|| {
            let (from, to) = (Self::key(from)?, Self::key(to)?);
            let mut files = self.files.lock().unwrap();
            let f = files.remove(&from).ok_or_else(|| format!("no such file: {from}"))?;
            files.insert(to, f);
            Ok(())
        })();
        Box::pin(async move { r })
    }

    fn remove(&self, path: &str, recursive: bool) -> BoxFuture<'_, StoreResult<()>> {
        let r = Self::key(path).map(|k| {
            let mut files = self.files.lock().unwrap();
            files.remove(&k);
            if recursive {
                let under = format!("{}/", k.trim_end_matches('/'));
                files.retain(|p, _| !p.starts_with(&under));
            }
        });
        Box::pin(async move { r })
    }

    fn stat(&self, path: &str) -> BoxFuture<'_, StoreResult<Option<StoredFile>>> {
        let r = Self::key(path).map(|k| {
            self.files.lock().unwrap().get(&k).map(|f| StoredFile { path: k.clone(), size: f.0.len() as u64, mtime_ms: f.1 })
        });
        Box::pin(async move { r })
    }

    fn list(&self, dir: &str) -> BoxFuture<'_, StoreResult<Vec<StoredFile>>> {
        let r = Self::key(dir).map(|k| {
            let under = format!("{}/", k.trim_end_matches('/'));
            self.files
                .lock()
                .unwrap()
                .iter()
                .filter(|(p, _)| p.starts_with(&under))
                .map(|(p, f)| StoredFile { path: p.clone(), size: f.0.len() as u64, mtime_ms: f.1 })
                .collect()
        });
        Box::pin(async move { r })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_never_leaves_the_store() {
        assert_eq!(relative("/sessions/a/b.jsonl").unwrap(), PathBuf::from("sessions/a/b.jsonl"));
        assert!(relative("/sessions/../../etc/passwd").is_err());
        assert!(relative("../x").is_err());
    }

    #[tokio::test]
    async fn a_directory_store_keeps_whole_files() {
        let dir = std::env::temp_dir().join(format!("fl_pi_llm-store-{}", std::process::id()));
        let s = DirectoryStore::new(&dir);
        s.append("/sessions/x/a.jsonl", "1\n".into()).await.unwrap();
        s.append("/sessions/x/a.jsonl", "2\n".into()).await.unwrap();
        assert_eq!(s.read("/sessions/x/a.jsonl", None).await.unwrap().as_deref(), Some("1\n2\n"));
        s.rename("/sessions/x/a.jsonl", "/sessions/y/b.jsonl").await.unwrap();
        let listed = s.list("/sessions").await.unwrap();
        assert_eq!(listed.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["/sessions/y/b.jsonl"]);
        assert_eq!(s.stat("/sessions/y/b.jsonl").await.unwrap().unwrap().size, 4);
        assert!(s.stat("/sessions/x/a.jsonl").await.unwrap().is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("sessions/y/b.jsonl")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        s.remove("/sessions", true).await.unwrap();
        assert!(s.list("/sessions").await.unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_memory_store_lists_under_a_directory_only() {
        let s = MemoryStore::new();
        s.write("/sessions/a.jsonl", "a".into()).await.unwrap();
        s.write("/sessionsx/b.jsonl", "b".into()).await.unwrap();
        assert_eq!(s.list("/sessions").await.unwrap().len(), 1);
        s.remove("/sessions", true).await.unwrap();
        assert!(s.read("/sessions/a.jsonl", None).await.unwrap().is_none());
        assert!(s.read("/sessionsx/b.jsonl", None).await.unwrap().is_some());
    }
}
