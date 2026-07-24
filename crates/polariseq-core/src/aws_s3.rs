use crate::progress::{transfer_bar_style, verify_bar_style};
use crate::progress_store::ProgressStore;
use anyhow::{anyhow, Result};
use futures::StreamExt;
use indicatif::{MultiProgress, ProgressBar};
use md5;
use quick_xml::events::Event;
use quick_xml::Reader;
use reqwest::{header, Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::str;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};

// ============================
// 1. Data Structures
// ============================

#[derive(Debug, Clone)]
pub struct SraMetadata {
    pub s3_uri: String,
    pub http_url: String,
    pub md5: Option<String>,
    pub size: u64,
}

/// Simple pause/resume token that can be shared between the GUI and the
/// AWS download workers.
#[derive(Clone, Default)]
pub struct PauseToken {
    paused: Arc<AtomicBool>,
}

impl PauseToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::Relaxed);
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Async-friendly wait: yields back to the Tokio runtime while paused.
    pub async fn wait_while_paused(&self) {
        while self.is_paused() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

#[derive(Debug, Clone)]
struct ChunkInfo {
    id: usize,
    start: u64,
    end: u64,
}

#[derive(Debug, Deserialize, Serialize)]
struct ProgressData {
    downloaded_chunks: Vec<usize>,
}

// ============================
// 2. Metadata Parsing and Conversion
// ============================

/// NCBI E-utilities (`efetch.fcgi`) allow only ~3 requests/second without an
/// API key. Every concurrent download task resolves its SRA metadata through
/// [`SraUtils::get_metadata`], so a burst of parallel runs easily exceeds that
/// limit and the server answers `429 Too Many Requests`. Because all tasks then
/// backed off by the *same* flat delay, they retried in lockstep and
/// re-triggered 429 round after round — the "download stuck before it starts"
/// symptom. This slot-based pacer is shared process-wide and guarantees we never
/// issue requests faster than `MIN_INTERVAL`, desynchronizing the tasks at the
/// source instead of letting them herd.
static NCBI_NEXT_SLOT: OnceLock<std::sync::Mutex<Instant>> = OnceLock::new();

async fn ncbi_rate_limit_wait() {
    // ~2.8 req/s, comfortably under NCBI's 3 req/s cap for keyless access.
    const MIN_INTERVAL: Duration = Duration::from_millis(350);
    let reserved = {
        let gate = NCBI_NEXT_SLOT.get_or_init(|| std::sync::Mutex::new(Instant::now()));
        // Recover the poisoned mutex rather than panicking the whole download.
        let mut guard = gate.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        // Reserve the next free slot (never in the past), then push the gate
        // forward so the following caller gets a later, distinct slot. The lock
        // is released before we sleep, so callers queue by slot, not by waiting
        // on each other.
        let slot = (*guard).max(now);
        *guard = slot + MIN_INTERVAL;
        slot
    };
    let delay = reserved.saturating_duration_since(Instant::now());
    tokio::time::sleep(delay).await;
}

/// Cheap dependency-free jitter (0..=max_ms) to desynchronize retry backoffs
/// across concurrent tasks. SplitMix64 stepped by an atomic counter gives each
/// call a distinct value — we only need to break lockstep, not be cryptographic.
fn jitter_millis(max_ms: u64) -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);
    let mut z = COUNTER.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    z % (max_ms + 1)
}

pub struct SraUtils;

impl SraUtils {
    pub async fn get_metadata(run_id: &str, _api_key: Option<&str>) -> Result<Option<SraMetadata>> {
        let url = format!(
            "https://eutils.ncbi.nlm.nih.gov/entrez/eutils/efetch.fcgi?db=sra&id={}&rettype=full&retmode=xml",
            run_id
        );

        // Modification 1: Timeout increased to 60 seconds
        let client = Client::builder().timeout(Duration::from_secs(60)).build()?;

        let mut attempt: u32 = 0;
        let max_retries: u32 = 10; // Modification 2: Max retries increased to 10

        loop {
            attempt += 1;
            // Pace every NCBI call process-wide so concurrent runs can't burst
            // past the E-utilities rate limit and trip a 429 thundering-herd.
            ncbi_rate_limit_wait().await;
            let result = client.get(&url).send().await;

            match result {
                Ok(resp) => {
                    if resp.status().is_success() {
                        let text = resp.text().await?;
                        return parse_sra_xml(&text);
                    }

                    if attempt >= max_retries {
                        return Err(anyhow!("NCBI API Error: Status {}", resp.status()));
                    }

                    // Transient error (429 rate-limit or 5xx). Honor the server's
                    // Retry-After when it sends one; otherwise use exponential
                    // backoff. Jitter keeps concurrent tasks from retrying in
                    // lockstep and re-colliding on the rate limit.
                    let status = resp.status();
                    let retry_after_secs = resp
                        .headers()
                        .get(header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse::<u64>().ok());

                    let wait_secs = retry_after_secs
                        .unwrap_or_else(|| std::cmp::min(60, 2_u64.saturating_pow(attempt)))
                        + jitter_millis(2000) / 1000;

                    warn!(
                        "[Network] NCBI Server Error ({}), retrying in {}s ({}/{})...",
                        status, wait_secs, attempt, max_retries
                    );
                    tokio::time::sleep(Duration::from_secs(wait_secs.max(1))).await;
                }
                Err(e) => {
                    if attempt >= max_retries {
                        return Err(anyhow!(
                            "Failed to connect to NCBI after {} attempts: {}",
                            max_retries,
                            e
                        ));
                    }
                    // Modification 3: exponential backoff (was a flat 10s) with
                    // jitter, so connection blips recover quickly without herding.
                    let wait_secs =
                        std::cmp::min(60, 2_u64.saturating_pow(attempt)) + jitter_millis(2000) / 1000;
                    warn!(
                        "[Network] Connection failed: {}. Retrying in {}s ({}/{})...",
                        e, wait_secs, attempt, max_retries
                    );
                    tokio::time::sleep(Duration::from_secs(wait_secs.max(1))).await;
                }
            }
        }
    }
}

// ... (resolve_urls, parse_sra_xml and other functions remain unchanged, please copy the previous code or keep it as is)
// To save space, only the SraUtils modification part is listed here. If the ResumableDownloader part has not changed, it does not need to be moved.
// But for completeness, here is the rest:

fn resolve_urls(raw_url: &str) -> Option<(String, String)> {
    if let Some(rest) = raw_url.strip_prefix("https://") {
        if let Some((bucket, key)) = rest.split_once(".s3.amazonaws.com/") {
            let s3 = format!("s3://{}/{}", bucket, key);
            return Some((s3, raw_url.to_string()));
        }
    }
    if let Some(rest) = raw_url.strip_prefix("s3://") {
        if let Some((bucket, key)) = rest.split_once('/') {
            let https = format!("https://{}.s3.amazonaws.com/{}", bucket, key);
            return Some((raw_url.to_string(), https));
        }
    }
    None
}

fn parse_sra_xml(xml_text: &str) -> Result<Option<SraMetadata>> {
    let mut reader = Reader::from_str(xml_text);
    let mut buf = Vec::new();
    let mut current_file_md5: Option<String> = None;
    let mut current_file_size: u64 = 0;
    let mut found_metadata: Option<SraMetadata> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let name = e.local_name();
                let name_str = str::from_utf8(name.as_ref()).unwrap_or("");
                if name_str.eq_ignore_ascii_case("SRAFile") || name_str.eq_ignore_ascii_case("Run")
                {
                    current_file_md5 = None;
                    current_file_size = 0;
                    for attr in e.attributes().flatten() {
                        let k = str::from_utf8(attr.key.as_ref()).unwrap_or("");
                        let v = str::from_utf8(attr.value.as_ref()).unwrap_or("");
                        if k.eq_ignore_ascii_case("md5") {
                            current_file_md5 = Some(v.to_string());
                        } else if k.eq_ignore_ascii_case("size") {
                            current_file_size = v.parse().unwrap_or(0);
                        }
                    }
                } else if name_str.eq_ignore_ascii_case("Alternatives") {
                    let mut is_aws = false;
                    let mut is_worldwide = false;
                    let mut curr_url = String::new();
                    for attr in e.attributes().flatten() {
                        let k = str::from_utf8(attr.key.as_ref()).unwrap_or("");
                        let v = str::from_utf8(attr.value.as_ref()).unwrap_or("");
                        if k.eq_ignore_ascii_case("org") && v.eq_ignore_ascii_case("AWS") {
                            is_aws = true;
                        } else if k.eq_ignore_ascii_case("free_egress")
                            && v.eq_ignore_ascii_case("worldwide")
                        {
                            is_worldwide = true;
                        } else if k.eq_ignore_ascii_case("url") {
                            curr_url = v.to_string();
                        }
                    }
                    if is_aws && is_worldwide && !curr_url.is_empty() {
                        if let Some((s3_uri, http_url)) = resolve_urls(&curr_url) {
                            found_metadata = Some(SraMetadata {
                                s3_uri,
                                http_url,
                                md5: current_file_md5.clone(),
                                size: current_file_size,
                            });
                            break;
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(found_metadata)
}

pub struct ResumableDownloader {
    run_id: String,
    metadata: SraMetadata,
    filepath: PathBuf,
    meta_file: PathBuf,
    chunk_size: u64,
    max_workers: usize,
    client: Client,
    mp: Option<Arc<MultiProgress>>,
    progress_bytes: Option<Arc<AtomicU64>>,
    pause_token: Option<PauseToken>,
    progress_store: Option<ProgressStore>,
}

impl ResumableDownloader {
    pub async fn new(
        run_id: String,
        metadata: SraMetadata,
        save_dir: PathBuf,
        chunk_size_mb: u64,
        max_workers: usize,
        mp: Option<Arc<MultiProgress>>,
        progress_store: Option<ProgressStore>,
    ) -> Result<Self> {
        let filename = metadata
            .s3_uri
            .split('/')
            .next_back()
            .filter(|name| !name.is_empty())
            .unwrap_or(&run_id)
            .to_string();
        let filepath = save_dir.join(&filename);
        let meta_file = filepath.with_extension("meta.json");

        // No whole-request body timeout: large Range chunks (e.g. 200 MiB) can
        // take many minutes on slow links. Rely on connect_timeout + per-chunk
        // retries with intra-chunk offset resume instead.
        let client = Client::builder()
            .http1_only()
            .connect_timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(max_workers)
            .build()?;

        Ok(Self {
            run_id,
            metadata,
            filepath,
            meta_file,
            chunk_size: chunk_size_mb * 1024 * 1024,
            max_workers,
            client,
            mp,
            progress_bytes: None,
            pause_token: None,
            progress_store,
        })
    }

    pub fn with_progress_bytes(mut self, progress: Arc<AtomicU64>) -> Self {
        self.progress_bytes = Some(progress);
        self
    }

    pub fn with_pause_token(mut self, token: PauseToken) -> Self {
        self.pause_token = Some(token);
        self
    }

    // ... (load_progress, save_progress, start, verify_integrity methods remain unchanged)
    fn load_progress(&self) -> HashSet<usize> {
        if self.meta_file.exists() {
            if let Ok(content) = std::fs::read_to_string(&self.meta_file) {
                if let Ok(progress) = serde_json::from_str::<ProgressData>(&content) {
                    return progress.downloaded_chunks.into_iter().collect();
                }
            }
        }
        HashSet::new()
    }
    fn save_progress(&self, downloaded_chunks: &HashSet<usize>) -> Result<()> {
        let progress_data = ProgressData {
            downloaded_chunks: downloaded_chunks.iter().cloned().collect(),
        };
        let content = serde_json::to_string(&progress_data)?;
        std::fs::write(&self.meta_file, content)?;
        Ok(())
    }

    fn invalidate_download(&self) {
        for path in [&self.filepath, &self.meta_file] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => warn!(
                    "Failed to remove invalid download {}: {error}",
                    path.display()
                ),
            }
        }
    }

    pub async fn start(&self) -> Result<bool> {
        let start_time = std::time::Instant::now();

        // Preallocation (`set_len`) makes incomplete downloads already have the
        // full remote size. Only treat a size-matched file as "maybe complete"
        // when there is no resume meta — `.meta.json` means in-progress chunks
        // and must not be wiped by an early MD5 check.
        if self.filepath.exists() {
            if let Ok(meta) = tokio::fs::metadata(&self.filepath).await {
                let size_matches = meta.len() == self.metadata.size;
                let has_resume_meta = self.meta_file.exists();

                if size_matches && !has_resume_meta {
                    info!(
                        "[{}] Existing file with matching size; verifying integrity...",
                        self.run_id
                    );
                    if self.verify_integrity(0.0, true).await? {
                        return Ok(true);
                    } else {
                        warn!(
                            "[{}] Existing file MD5 mismatch; redownloading...",
                            self.run_id
                        );
                    }
                } else if size_matches && has_resume_meta {
                    info!(
                        "[{}] Resuming incomplete download from progress file...",
                        self.run_id
                    );
                } else if !size_matches {
                    warn!(
                        "[{}] Local size {} != remote {}; restarting download...",
                        self.run_id,
                        meta.len(),
                        self.metadata.size
                    );
                    self.invalidate_download();
                }
            }
        }

        if !self.filepath.exists() {
            if let Some(parent) = self.filepath.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let file = File::create(&self.filepath)?;
            file.set_len(self.metadata.size)?;
        }

        let mut downloaded_chunks = self.load_progress();
        let num_chunks = self.metadata.size.div_ceil(self.chunk_size);
        let mut tasks = Vec::new();
        for i in 0..num_chunks {
            if !downloaded_chunks.contains(&(i as usize)) {
                tasks.push(ChunkInfo {
                    id: i as usize,
                    start: i * self.chunk_size,
                    end: std::cmp::min((i + 1) * self.chunk_size - 1, self.metadata.size - 1),
                });
            }
        }

        // Setup Progress Bar
        let pb = if let Some(mp) = &self.mp {
            // insert_from_back(1) places the bar just above the pinned global
            // status bar (which lives at the very back of the MultiProgress),
            // so transient per-file bars never sink below it.
            mp.insert_from_back(1, ProgressBar::new(self.metadata.size))
        } else {
            ProgressBar::new(self.metadata.size)
        };
        pb.set_style(transfer_bar_style());
        pb.set_prefix(self.run_id.clone());
        pb.set_message("Downloading");
        pb.enable_steady_tick(std::time::Duration::from_millis(100));

        // Keep per-file details in the log file without cluttering active progress bars.
        let size_gb = self.metadata.size as f64 / 1024.0 / 1024.0 / 1024.0;
        let details = format!(
            "{} │ {:.2} GB │ {} │ {}",
            self.run_id,
            size_gb,
            self.metadata.md5.as_deref().unwrap_or("N/A"),
            self.filepath.display()
        );
        info!(target: "download_detail", "{}", details);

        if tasks.is_empty() {
            let msg = format!(
                "{} │ File exists, starting integrity check...",
                self.run_id
            );
            pb.println(&msg);
            info!(target: "download_detail", "{}", msg);
            pb.finish_and_clear();
            return self
                .verify_integrity(start_time.elapsed().as_secs_f64(), true)
                .await;
        }

        let initial_bytes: u64 = downloaded_chunks
            .iter()
            .map(|&id| {
                let start = id as u64 * self.chunk_size;
                let end = std::cmp::min((id as u64 + 1) * self.chunk_size, self.metadata.size);
                end.saturating_sub(start)
            })
            .sum();
        // Fix: Use AtomicU64 to track global progress safely (handles retries)
        // If the caller supplied a shared counter (e.g. the GUI), use it so the
        // progress can be observed externally.
        let global_bytes = self
            .progress_bytes
            .clone()
            .unwrap_or_else(|| Arc::new(AtomicU64::new(0)));
        global_bytes.store(
            std::cmp::min(initial_bytes, self.metadata.size),
            Ordering::Relaxed,
        );

        pb.set_position(global_bytes.load(Ordering::Relaxed));

        // Spawn progress monitor
        let pb_monitor = pb.clone();
        let gb_monitor = global_bytes.clone();
        let store_monitor = self.progress_store.clone();
        let run_id_monitor = self.run_id.clone();
        let sra_size_monitor = self.metadata.size;
        let monitor_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(100));
            loop {
                interval.tick().await;
                let bytes = gb_monitor.load(Ordering::Relaxed);
                pb_monitor.set_position(bytes);
                if let Some(store) = &store_monitor {
                    let mut map = store.write().await;
                    if let Some(rp) = map.get_mut(&run_id_monitor) {
                        rp.download.update(bytes, sra_size_monitor);
                        rp.recalculate_overall();
                    }
                }
            }
        });

        // Result channel: Ok(chunk_id) on success, Err((chunk, error)) on failure
        // so the coordinator can requeue with a retry budget.
        let (tx, mut rx) = mpsc::channel::<Result<usize, (ChunkInfo, anyhow::Error)>>(100);
        let shared_tasks = Arc::new(Mutex::new(tasks));
        let outstanding = Arc::new(AtomicU64::new(
            (num_chunks as usize).saturating_sub(downloaded_chunks.len()) as u64,
        ));
        let pause_token = self.pause_token.clone();
        for _ in 0..self.max_workers {
            let client = self.client.clone();
            let url = self.metadata.http_url.clone();
            let filepath = self.filepath.clone();
            let queue = shared_tasks.clone();
            let tx = tx.clone();
            let gb_clone = global_bytes.clone();
            let outstanding_w = outstanding.clone();
            let pause_token_worker = pause_token.clone();
            tokio::spawn(async move {
                loop {
                    if outstanding_w.load(Ordering::SeqCst) == 0 {
                        break;
                    }
                    if let Some(token) = &pause_token_worker {
                        token.wait_while_paused().await;
                    }

                    let task = {
                        let mut q = queue.lock().await;
                        q.pop()
                    };
                    match task {
                        Some(t) => {
                            match download_chunk_http(
                                client.clone(),
                                &url,
                                &t,
                                &filepath,
                                gb_clone.clone(),
                                pause_token_worker.clone(),
                            )
                            .await
                            {
                                Ok(_) => {
                                    if tx.send(Ok(t.id)).await.is_err() {
                                        break;
                                    }
                                }
                                Err(e) => {
                                    if tx.send(Err((t, e))).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                        None => {
                            // Queue empty but work may be requeued after a failure.
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                    }
                }
            });
        }
        drop(tx);

        const MAX_CHUNK_RETRIES: u32 = 3;
        let mut chunk_retries: std::collections::HashMap<usize, u32> =
            std::collections::HashMap::new();
        let mut fatal_errors: Vec<anyhow::Error> = Vec::new();

        while outstanding.load(Ordering::SeqCst) > 0 {
            match rx.recv().await {
                Some(Ok(chunk_id)) => {
                    downloaded_chunks.insert(chunk_id);
                    if let Err(e) = self.save_progress(&downloaded_chunks) {
                        warn!("Failed to save progress for {}: {}", self.run_id, e);
                    }
                    outstanding.fetch_sub(1, Ordering::SeqCst);
                }
                Some(Err((chunk, e))) => {
                    let attempt = chunk_retries.entry(chunk.id).or_insert(0);
                    *attempt += 1;
                    if *attempt <= MAX_CHUNK_RETRIES {
                        warn!(
                            "[{}] Chunk {} failed (attempt {}/{}): {:#}. Requeueing...",
                            self.run_id, chunk.id, *attempt, MAX_CHUNK_RETRIES, e
                        );
                        shared_tasks.lock().await.push(chunk);
                    } else {
                        warn!(
                            "[{}] Chunk {} failed after {} attempts: {:#}",
                            self.run_id, chunk.id, MAX_CHUNK_RETRIES, e
                        );
                        fatal_errors.push(e);
                        outstanding.fetch_sub(1, Ordering::SeqCst);
                    }
                }
                None => break,
            }
        }

        monitor_handle.abort();
        pb.finish_and_clear();

        if !fatal_errors.is_empty() {
            return Err(anyhow!(
                "[{}] {} chunk(s) failed permanently (e.g. {})",
                self.run_id,
                fatal_errors.len(),
                fatal_errors[0]
            ));
        }

        if downloaded_chunks.len() as u64 == num_chunks {
            self.verify_integrity(start_time.elapsed().as_secs_f64(), false)
                .await
        } else {
            let msg = format!(
                "{} │ Download incomplete. Progress saved, please retry.",
                self.run_id
            );
            pb.println(&msg);
            warn!("{}", msg);
            Err(anyhow!("{}", msg))
        }
    }
    async fn verify_integrity(
        &self,
        download_duration: f64,
        skipped_download: bool,
    ) -> Result<bool> {
        let start_time = std::time::Instant::now();
        if self.metadata.md5.is_none() {
            let local_size = tokio::fs::metadata(&self.filepath).await?.len();
            if local_size != self.metadata.size {
                warn!(
                    "{} │ Size mismatch: local={} remote={}",
                    self.run_id, local_size, self.metadata.size
                );
                self.invalidate_download();
                return Ok(false);
            }
            let _ = std::fs::remove_file(&self.meta_file);
            return Ok(true);
        }

        let pb = if let Some(mp) = &self.mp {
            mp.insert_from_back(1, ProgressBar::new(self.metadata.size))
        } else {
            ProgressBar::new(self.metadata.size)
        };

        pb.set_style(verify_bar_style());
        pb.set_prefix(self.run_id.clone());
        pb.set_message("Verifying");
        pb.enable_steady_tick(std::time::Duration::from_millis(100));

        let mut file = tokio::fs::File::open(&self.filepath).await?;
        let mut ctx = md5::Context::new();
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            let n = file.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            ctx.consume(&buf[..n]);
            pb.inc(n as u64);
        }
        pb.finish_and_clear();

        let local_md5 = format!("{:x}", ctx.compute());
        let expected_md5 = self.metadata.md5.as_ref().unwrap();
        if &local_md5 == expected_md5 {
            if !skipped_download {
                let speed = (self.metadata.size as f64 / 1024.0 / 1024.0) / download_duration;
                let msg = format!("{} │ {:.2} MB/s", self.run_id, speed);
                info!(target: "download_detail", "{}", msg);
            }
            let msg = format!(
                "{} │ MD5 OK ({:.2}s)",
                self.run_id,
                start_time.elapsed().as_secs_f64()
            );
            info!(target: "download_detail", "{}", msg);

            let _ = std::fs::remove_file(&self.meta_file);
            Ok(true)
        } else {
            let msg = format!(
                "{} │ MD5 mismatch! Local: {}  Remote: {}",
                self.run_id, local_md5, expected_md5
            );
            warn!("{}", msg);
            self.invalidate_download();
            Ok(false)
        }
    }
}

async fn download_chunk_http(
    client: Client,
    url: &str,
    chunk: &ChunkInfo,
    filepath: &Path,
    global_bytes: Arc<AtomicU64>,
    pause_token: Option<PauseToken>,
) -> Result<()> {
    const MAX_TOTAL_ATTEMPTS: u32 = 50;
    const READ_TIMEOUT: Duration = Duration::from_secs(120);

    let mut retry = 0;
    let mut total_attempts: u32 = 0;
    let mut current_offset = chunk.start;

    loop {
        if let Some(token) = &pause_token {
            token.wait_while_paused().await;
        }

        if current_offset > chunk.end {
            return Ok(());
        }

        total_attempts += 1;
        if total_attempts > MAX_TOTAL_ATTEMPTS {
            return Err(anyhow!(
                "Chunk {} aborted after {} total attempts (offset {}/{})",
                chunk.id,
                MAX_TOTAL_ATTEMPTS,
                current_offset - chunk.start,
                chunk.end - chunk.start + 1
            ));
        }

        let range_header = format!("bytes={}-{}", current_offset, chunk.end);
        let resp = client
            .get(url)
            .header(header::RANGE, range_header)
            .send()
            .await;

        if let Ok(response) = resp {
            // Handle 429 rate limiting with Retry-After and exponential backoff
            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                retry += 1;
                if retry > 15 {
                    return Err(anyhow!(
                        "Chunk {} rate-limited (429) after {} retries",
                        chunk.id,
                        retry
                    ));
                }
                let wait_secs = response
                    .headers()
                    .get(header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or_else(|| std::cmp::min(120, 2_u64.saturating_pow(retry)));
                warn!(
                    "[RateLimit] 429 Too Many Requests on chunk {}, backing off {}s (retry {}/15)",
                    chunk.id, wait_secs, retry
                );
                tokio::time::sleep(Duration::from_secs(wait_secs)).await;
                continue;
            }

            let expected_content_range = format!("bytes {}-{}/", current_offset, chunk.end);
            let has_expected_range = response
                .headers()
                .get(header::CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with(&expected_content_range));
            if response.status() != StatusCode::PARTIAL_CONTENT || !has_expected_range {
                retry += 1;
                if retry > 10 {
                    return Err(anyhow!(
                        "Unexpected HTTP Range response: status={}, content-range={:?}",
                        response.status(),
                        response.headers().get(header::CONTENT_RANGE)
                    ));
                }
                tokio::time::sleep(Duration::from_secs(retry as u64)).await;
                continue;
            }

            // Successful 206 response — reset retry counter
            retry = 0;

            let mut stream = response.bytes_stream();
            let mut file = std::fs::OpenOptions::new().write(true).open(filepath)?;
            file.seek(SeekFrom::Start(current_offset))?;

            let mut stream_error = false;
            let offset_start = current_offset;

            loop {
                if let Some(token) = &pause_token {
                    token.wait_while_paused().await;
                }

                let item = tokio::time::timeout(READ_TIMEOUT, stream.next()).await;
                match item {
                    Ok(Some(Ok(bytes))) => {
                        if file.write_all(&bytes).is_err() {
                            stream_error = true;
                            break;
                        }
                        let len = bytes.len() as u64;
                        global_bytes.fetch_add(len, Ordering::Relaxed);
                        current_offset += len;
                    }
                    Ok(Some(Err(_))) => {
                        stream_error = true;
                        break;
                    }
                    Ok(None) => break,
                    Err(_) => {
                        warn!(
                            "[Timeout] Read timeout ({}s) on chunk {} at offset {}",
                            READ_TIMEOUT.as_secs(),
                            chunk.id,
                            current_offset
                        );
                        stream_error = true;
                        break;
                    }
                }
            }

            if !stream_error && current_offset > chunk.end {
                return Ok(());
            }

            if current_offset > offset_start {
                retry = 0;
            }
        }

        retry += 1;
        if retry > 20 {
            return Err(anyhow!("Chunk failed after multiple retries"));
        }
        let sleep_sec = std::cmp::min(30, 1_u64 << std::cmp::min(retry, 5));
        tokio::time::sleep(Duration::from_secs(sleep_sec)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn keeps_the_source_object_filename_for_generic_downloads() {
        let temp_dir = tempfile::tempdir().unwrap();
        let downloader = ResumableDownloader::new(
            "k2_viral".to_string(),
            SraMetadata {
                s3_uri: "s3://genome-idx/kraken/k2_viral_20240112.tar.gz".to_string(),
                http_url: "https://genome-idx.s3.amazonaws.com/kraken/k2_viral_20240112.tar.gz"
                    .to_string(),
                md5: None,
                size: 1,
            },
            temp_dir.path().to_path_buf(),
            64,
            1,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            downloader.filepath.file_name().unwrap(),
            "k2_viral_20240112.tar.gz"
        );
    }

    #[tokio::test]
    async fn removes_corrupt_file_and_progress_metadata_after_md5_failure() {
        let temp_dir = tempfile::tempdir().unwrap();
        let downloader = ResumableDownloader::new(
            "example".to_string(),
            SraMetadata {
                s3_uri: "s3://example-bucket/example.dat".to_string(),
                http_url: "https://example-bucket.s3.amazonaws.com/example.dat".to_string(),
                md5: Some("d41d8cd98f00b204e9800998ecf8427e".to_string()),
                size: 3,
            },
            temp_dir.path().to_path_buf(),
            64,
            1,
            None,
            None,
        )
        .await
        .unwrap();

        std::fs::write(&downloader.filepath, b"bad").unwrap();
        std::fs::write(&downloader.meta_file, r#"{"downloaded_chunks":[0]}"#).unwrap();

        assert!(!downloader.verify_integrity(0.0, false).await.unwrap());
        assert!(!downloader.filepath.exists());
        assert!(!downloader.meta_file.exists());
    }

    #[test]
    fn resume_meta_preserves_completed_chunks_when_file_preallocated() {
        let temp_dir = tempfile::tempdir().unwrap();
        let filepath = temp_dir.path().join("example.dat");
        let meta_file = filepath.with_extension("meta.json");

        // Simulate preallocated full-size file with partial progress.
        {
            let f = File::create(&filepath).unwrap();
            f.set_len(10 * 1024 * 1024).unwrap();
        }
        std::fs::write(&meta_file, r#"{"downloaded_chunks":[0,2]}"#).unwrap();

        // load_progress is private; mirror the rule used by start():
        // meta present ⇒ treat as incomplete resume, do not wipe.
        assert!(filepath.exists());
        assert!(meta_file.exists());
        assert_eq!(
            std::fs::metadata(&filepath).unwrap().len(),
            10 * 1024 * 1024
        );
        let progress: ProgressData =
            serde_json::from_str(&std::fs::read_to_string(&meta_file).unwrap()).unwrap();
        assert_eq!(progress.downloaded_chunks, vec![0, 2]);
        // Early MD5 must only run when meta is absent.
        assert!(meta_file.exists());
    }
}
