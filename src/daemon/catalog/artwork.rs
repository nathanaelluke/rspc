//! Cover images survive navigation and daemon restarts without growing the cache indefinitely.
use super::*;
use std::{
    io::Read,
    path::{Path, PathBuf},
    time::{Instant, SystemTime},
};

const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_MEMORY_BYTES: usize = 32 * 1024 * 1024;
const MAX_MEMORY_IMAGES: usize = 64;
const MAX_DISK_BYTES: u64 = 128 * 1024 * 1024;
const MAX_DISK_FILES: usize = 512;

#[derive(Default)]
pub(super) struct Memory {
    entries: HashMap<String, (Instant, Arc<Vec<u8>>)>,
    bytes: usize,
}

impl Memory {
    fn get(&mut self, uri: &str) -> Option<Arc<Vec<u8>>> {
        let (used, data) = self.entries.get_mut(uri)?;
        *used = Instant::now();
        Some(Arc::clone(data))
    }

    fn insert(&mut self, uri: String, data: Arc<Vec<u8>>, limit: usize) {
        if data.len() > limit {
            return;
        }
        if let Some((_, old)) = self.entries.remove(&uri) {
            self.bytes -= old.len();
        }
        while self.bytes + data.len() > limit || self.entries.len() >= MAX_MEMORY_IMAGES {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (used, _))| *used)
                .map(|(uri, _)| uri.clone())
            else {
                return;
            };
            let (_, old) = self.entries.remove(&oldest).unwrap();
            self.bytes -= old.len();
        }
        self.bytes += data.len();
        self.entries.insert(uri, (Instant::now(), data));
    }
}

impl Catalog {
    pub(super) fn artwork_file(&self, uri: &str) -> PathBuf {
        self.disk_root
            .join("artwork")
            .join(self.disk(uri).with_extension("bin").file_name().unwrap())
    }

    pub async fn artwork(self: &Arc<Self>, uri: &str) -> Result<Arc<Vec<u8>>> {
        let url = url::Url::parse(uri)?;
        ensure!(url.scheme() == "https", "artwork must use HTTPS");
        if let Some(data) = self.artwork.lock().await.get(uri) {
            return Ok(data);
        }
        let gate = self.request_gate(&format!("artwork:{uri}")).await;
        let _guard = gate.lock().await;
        if let Some(data) = self.artwork.lock().await.get(uri) {
            return Ok(data);
        }
        let file = self.artwork_file(uri);
        let cached = tokio::task::spawn_blocking({
            let file = file.clone();
            move || read_cached(&file)
        })
        .await?;
        let data = match cached {
            Some(data) => Arc::new(data),
            None => {
                let _foreground = background::ForegroundRequest::new(&self.foreground);
                let _permit = self.request_permit().await?;
                let response = crate::spotify::http()
                    .get(url)
                    .send()
                    .await?
                    .error_for_status()?;
                let data = Arc::new(artwork_bytes(response).await?);
                let stored = Arc::clone(&data);
                let _writes = self.disk_writes.lock().await;
                let result = tokio::task::spawn_blocking(move || -> Result<()> {
                    crate::paths::write_private_file_atomic(&file, stored.as_slice())?;
                    trim_disk(file.parent().unwrap(), MAX_DISK_BYTES, MAX_DISK_FILES)
                })
                .await?;
                if let Err(error) = result {
                    log::debug!("could not persist artwork: {error}");
                }
                data
            }
        };
        self.artwork
            .lock()
            .await
            .insert(uri.to_owned(), Arc::clone(&data), MAX_MEMORY_BYTES);
        Ok(data)
    }
}

fn read_cached(path: &Path) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_IMAGE_BYTES as u64 {
        return None;
    }
    let _ = file.set_modified(SystemTime::now());
    let mut data = Vec::new();
    file.take(MAX_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut data)
        .ok()?;
    (data.len() <= MAX_IMAGE_BYTES).then_some(data)
}

fn trim_disk(directory: &Path, max_bytes: u64, max_files: usize) -> Result<()> {
    let mut files = Vec::new();
    let mut bytes = 0;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if entry.path().extension().is_none_or(|ext| ext != "bin") {
            continue;
        }
        let metadata = entry.metadata()?;
        bytes += metadata.len();
        files.push((metadata.modified()?, metadata.len(), entry.path()));
    }
    files.sort_unstable();
    let mut count = files.len();
    for (_, size, path) in files {
        if bytes <= max_bytes && count <= max_files {
            break;
        }
        std::fs::remove_file(path)?;
        bytes -= size;
        count -= 1;
    }
    Ok(())
}

async fn artwork_bytes(mut response: reqwest::Response) -> Result<Vec<u8>> {
    let length = response.content_length();
    ensure!(
        length.is_none_or(|length| length <= MAX_IMAGE_BYTES as u64),
        "album artwork is too large"
    );
    let mut data = Vec::with_capacity(length.unwrap_or(0) as usize);
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            chunk.len() <= MAX_IMAGE_BYTES - data.len(),
            "album artwork is too large"
        );
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn covers_load_from_disk_after_restart_without_network_access() {
        let dir = tempfile::tempdir().unwrap();
        let mut catalog = Catalog::new(false);
        catalog.disk_root = dir.path().to_owned();
        let uri = "https://unreachable.invalid/cover";
        crate::paths::write_private_file_atomic(&catalog.artwork_file(uri), b"cached cover")
            .unwrap();
        let catalog = Arc::new(catalog);
        let data = tokio::time::timeout(Duration::from_millis(100), catalog.artwork(uri))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(data.as_slice(), b"cached cover");
        std::fs::remove_file(catalog.artwork_file(uri)).unwrap();
        assert!(Arc::ptr_eq(&data, &catalog.artwork(uri).await.unwrap()));
    }

    #[test]
    fn memory_eviction_keeps_recent_covers_instead_of_clearing_everything() {
        let mut cache = Memory::default();
        cache.insert("first".into(), Arc::new(vec![1; 4]), 8);
        cache.insert("second".into(), Arc::new(vec![2; 4]), 8);
        let first = cache.get("first").unwrap();
        cache.insert("third".into(), Arc::new(vec![3; 4]), 8);
        assert!(cache.get("second").is_none());
        assert!(Arc::ptr_eq(&first, &cache.get("first").unwrap()));
        assert!(cache.get("third").is_some());
        assert_eq!(cache.bytes, 8);
    }

    #[test]
    fn disk_eviction_respects_bytes_and_file_count_and_keeps_recent_reads() {
        let dir = tempfile::tempdir().unwrap();
        for (name, age) in [("first", 3), ("second", 2), ("third", 1)] {
            let file = dir.path().join(format!("{name}.bin"));
            std::fs::write(&file, b"four").unwrap();
            std::fs::File::open(&file)
                .unwrap()
                .set_modified(SystemTime::now() - Duration::from_secs(age))
                .unwrap();
        }
        read_cached(&dir.path().join("first.bin")).unwrap();
        trim_disk(dir.path(), 8, 3).unwrap();
        assert!(!dir.path().join("second.bin").exists());
        assert!(dir.path().join("first.bin").exists());
        trim_disk(dir.path(), 8, 1).unwrap();
        assert!(!dir.path().join("third.bin").exists());
        assert!(dir.path().join("first.bin").exists());
    }
    #[tokio::test]
    async fn artwork_downloads_are_bounded_without_a_content_length() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut connection, _) = listener.accept().await.unwrap();
            // Reading one request line suffices for this deliberately incomplete
            // HTTP fixture; the response body ends when its connection closes.
            let mut request = BufReader::new(&mut connection);
            let mut line = String::new();
            request.read_line(&mut line).await.unwrap();
            connection
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let chunk = vec![0; 1024 * 1024];
            for _ in 0..17 {
                if connection.write_all(&chunk).await.is_err() {
                    break;
                }
            }
        });
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{address}/cover"))
            .send()
            .await
            .unwrap();
        assert!(response.content_length().is_none());
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(2), artwork_bytes(response))
                .await
                .unwrap();
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("artwork is too large")
        );
        server.await.unwrap();
    }
}
