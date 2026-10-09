//! `gsa-download` subcommand: download GSA (NGDC CNCB) sequencing data.
//!
//! Downloads are FTP-first (per project decision): each file is fetched with
//! an external `wget -c` subprocess — the same resumable pattern as the EBI
//! FTP pipeline in `polariseq_core::ftp` — with a Semaphore bounding
//! concurrency, a 500ms file-size polling progress bar, and post-download MD5
//! verification against the CRA `md5sum.txt` manifest. No AWS S3 downloader
//! is used.

use anyhow::{anyhow, Context, Result};
use indicatif::{MultiProgress, ProgressBar};
use polariseq_core::gsa::{
    build_download_tasks, parse_gsa_input, resolve_inputs, GsaDownloadTask,
};
use polariseq_core::md5::verify_md5_entries;
use polariseq_core::progress::{spinner_style, transfer_bar_style};
use polariseq_core::observer::DownloadObserver;
use polariseq_core::{generate_md5sum_file_at};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::fs;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

use crate::ui_manager::UiManager;

/// Threads used for post-download MD5 verification.
const MD5_VERIFY_THREADS: usize = 4;

#[derive(clap::Args, Debug)]
#[command(arg_required_else_help = true)]
pub struct GsaDownloadArgs {
    /// GSA accessions or browse URLs, e.g. CRA017017 CRR1235475
    /// https://ngdc.cncb.ac.cn/gsa/browse/CRA017017/CRR1235475
    #[arg(value_name = "ACCESSION", num_args = 1.., help_heading = "Input Options")]
    pub accessions: Vec<String>,

    /// Output directory (default: ./gsa_download)
    #[arg(short, long, value_name = "DIR", help_heading = "Input Options")]
    pub output: Option<PathBuf>,

    /// Number of files to download concurrently
    #[arg(
        short = 'p',
        long = "multithreads",
        default_value_t = 2,
        help_heading = "Download Options"
    )]
    pub multithreads: usize,

    /// Skip *_sta.xml run metadata files, download sequence files only
    #[arg(long = "seq-only", help_heading = "Download Options")]
    pub seq_only: bool,

    /// Print the resolved file list without downloading
    #[arg(long, help_heading = "Download Options")]
    pub dry_run: bool,
}

impl GsaDownloadArgs {
    fn output_dir(&self) -> PathBuf {
        self.output.clone().unwrap_or_else(|| PathBuf::from("gsa_download"))
    }
}

/// Entry point wired into main.rs. `ui` is provided by the caller when
/// progress bars are active.
pub async fn run(args: &GsaDownloadArgs, ui: Option<Arc<UiManager>>) -> Result<()> {
    if args.multithreads == 0 {
        return Err(anyhow!("--multithreads must be at least 1"));
    }

    // ---- Stage 1: parse input ----
    let mut inputs = Vec::new();
    for acc in &args.accessions {
        inputs.push(parse_gsa_input(acc)?);
    }

    // ---- Stage 2: resolve runs to file lists (FTP listing + search page) ----
    let runs = resolve_inputs(&inputs).await?;
    let tasks = build_download_tasks(&runs, args.seq_only);
    let total_bytes: u64 = tasks.iter().map(|t| t.total_size).sum();
    info!(
        "Resolved {} file(s) from {} run(s), {} total",
        tasks.len(),
        runs.len(),
        indicatif::HumanBytes(total_bytes)
    );

    if args.dry_run {
        for task in &tasks {
            println!(
                "{}\t{}\t{}",
                task.crr,
                task.filename,
                task.md5.as_deref().unwrap_or("-")
            );
            println!("  {}", task.url);
        }
        info!("Dry run: {} file(s) listed, nothing downloaded", tasks.len());
        return Ok(());
    }

    // ---- Stage 3: concurrent wget -c downloads (FTP-first) ----
    let output_dir = args.output_dir();
    fs::create_dir_all(&output_dir)
        .await
        .with_context(|| format!("Failed to create output directory {}", output_dir.display()))?;

    if let Some(ui) = &ui {
        ui.set_total(tasks.len() as u64);
    }

    download_tasks(&tasks, &output_dir, args.multithreads, ui.as_ref()).await?;

    // ---- Stage 4: MD5 verification (files with a known md5 only) ----
    let entries: Vec<(String, String)> = tasks
        .iter()
        .filter_map(|t| {
            t.md5
                .as_ref()
                .map(|md5| (md5.clone(), t.filename.clone()))
        })
        .collect();
    if entries.is_empty() {
        warn!("No md5 checksums available; skipping verification");
    } else {
        info!("Verifying MD5 for {} downloaded file(s)...", entries.len());
        let progress = ui.map(|ui| Arc::new(ui.multi_progress()));
        let (passed, failed_count) =
            verify_md5_entries(entries, &output_dir, MD5_VERIFY_THREADS, progress).await?;
        if failed_count > 0 {
            return Err(anyhow!(
                "MD5 verification failed for {} of {} file(s)",
                failed_count,
                passed + failed_count
            ));
        }
        info!("MD5 verification passed for {} file(s)", passed);
    }

    // ---- Stage 5: md5sum-compatible manifest of downloaded files ----
    let mut manifest_files = Vec::new();
    for task in &tasks {
        let path = output_dir.join(&task.filename);
        if path.exists() {
            manifest_files.push(path);
        }
    }
    if !manifest_files.is_empty() {
        generate_md5sum_file_at(&output_dir.join("md5.txt"), &manifest_files)?;
    }

    info!("GSA download completed successfully");
    Ok(())
}

/// Run all download tasks with bounded concurrency; returns the number of
/// failures (errors are also logged). Mirrors `polariseq_core::ftp`.
async fn download_tasks(
    tasks: &[GsaDownloadTask],
    output_dir: &Path,
    threads: usize,
    ui: Option<&Arc<UiManager>>,
) -> Result<usize> {
    let semaphore = Arc::new(Semaphore::new(threads));
    let mp = ui
        .map(|ui| Arc::new(ui.multi_progress()))
        .unwrap_or_else(|| Arc::new(MultiProgress::new()));
    let mut handles = Vec::new();

    for task in tasks {
        let sem = semaphore.clone();
        let mp = mp.clone();
        let output_dir = output_dir.to_path_buf();
        let t_url = task.url.clone();
        let t_file = task.filename.clone();
        let t_crr = task.crr.clone();
        let t_size = task.total_size;
        let t_md5 = task.md5.clone();

        let handle = tokio::spawn(async move {
            let _permit = sem.acquire().await.expect("semaphore closed");

            let pb = if t_size > 0 {
                let p = mp.add(ProgressBar::new(t_size));
                p.set_style(transfer_bar_style());
                p
            } else {
                let p = mp.add(ProgressBar::new_spinner());
                p.set_style(spinner_style());
                p
            };
            pb.set_prefix(format!("{t_crr}/{t_file}"));
            pb.enable_steady_tick(Duration::from_millis(120));

            let output_file_path = output_dir.join(&t_file);

            // Existing-file short circuit: size match + md5 match = done.
            if output_file_path.exists() {
                if let Ok(meta) = fs::metadata(&output_file_path).await {
                    if t_size > 0 && meta.len() == t_size {
                        if let Some(expected) = &t_md5 {
                            if let Ok(true) = verify_md5(&output_file_path, expected).await {
                                pb.finish_and_clear();
                                return Ok(());
                            }
                        } else {
                            // Size matches and no manifest md5: accept as done.
                            pb.finish_and_clear();
                            return Ok(());
                        }
                    } else if meta.len() > 0 {
                        // Partial download: seed progress, wget -c resumes.
                        pb.set_position(meta.len());
                    }
                }
            }

            pb.set_message("Downloading");

            // Background monitor: file size every 500ms drives the bar.
            let monitor_path = output_file_path.clone();
            let monitor_pb = pb.clone();
            let monitor_handle = tokio::spawn(async move {
                loop {
                    sleep(Duration::from_millis(500)).await;
                    if let Ok(meta) = fs::metadata(&monitor_path).await {
                        monitor_pb.set_position(meta.len());
                    }
                }
            });

            let output = Command::new("wget")
                .args(["-c", &t_url])
                .current_dir(&output_dir)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output()
                .await;

            monitor_handle.abort();

            match output {
                Ok(out) => {
                    if !out.status.success() {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        pb.finish_with_message(format!("Failed (Exit {})", out.status));
                        error!("wget -c {} failed: {}", t_url, stderr.trim());
                        return Err(anyhow!("Download failed: {t_file}"));
                    }
                }
                Err(e) => {
                    pb.finish_with_message(format!("Exec Error: {e}"));
                    return Err(anyhow::anyhow!(e));
                }
            }

            if t_size > 0 {
                pb.set_position(t_size);
            }

            // Post-download verification when a manifest md5 is known.
            if let Some(expected) = &t_md5 {
                pb.set_message("Verifying MD5");
                match verify_md5(&output_file_path, expected).await {
                    Ok(true) => pb.finish_and_clear(),
                    Ok(false) => {
                        pb.finish_with_message("MD5 Mismatch");
                        warn!(
                            "MD5 mismatch for {}: expected {}",
                            t_file, expected
                        );
                        return Err(anyhow!("MD5 mismatch: {t_file}"));
                    }
                    Err(e) => {
                        pb.finish_with_message(format!("Check Error: {e}"));
                        return Err(e);
                    }
                }
            } else {
                pb.finish_and_clear();
            }
            Ok(())
        });
        handles.push(handle);
    }

    let mut failed = 0usize;
    let mut first_err: Option<anyhow::Error> = None;
    for handle in handles {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                failed += 1;
                warn!("GSA download task failed: {:#}", e);
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
            Err(e) => {
                failed += 1;
                warn!("GSA download task join error: {}", e);
                if first_err.is_none() {
                    first_err = Some(anyhow::anyhow!("task join error: {e}"));
                }
            }
        }
    }

    mp.clear().ok();
    if failed > 0 {
        if failed == tasks.len() {
            // Every task failed — surface the first error for diagnostics.
            return Err(first_err
                .unwrap_or_else(|| anyhow!("all {failed} GSA download task(s) failed")));
        }
        return Err(anyhow!(
            "{} of {} GSA download task(s) failed: {}",
            failed,
            tasks.len(),
            first_err.map(|e| e.to_string()).unwrap_or_default()
        ));
    }
    Ok(0)
}

/// Streaming MD5 check of a local file (same approach as `ftp::verify_md5`).
async fn verify_md5(path: &Path, expected: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let mut file = fs::File::open(path).await?;
    let mut context = ::md5::Context::new();
    let mut buffer = vec![0; 1024 * 1024 * 4];
    loop {
        let n = file.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        context.consume(&buffer[..n]);
    }
    let digest = context.compute();
    Ok(format!("{digest:x}") == expected)
}
