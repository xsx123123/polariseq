use anyhow::{anyhow, Context, Result};
use clap::{Args, ValueEnum};
use futures_util::{stream, StreamExt};
use reqwest::header::{CONTENT_RANGE, RANGE};
use reqwest::{Client, Url};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

use crate::ui_manager::UiManager;
use polariseq_core::md5::{parse_md5_manifest, verify_md5_entries};
use polariseq_core::observer::{CompletedInfo, DownloadObserver};

const NCBI_EUTILS_SEARCH_ENDPOINT: &str =
    "https://eutils.ncbi.nlm.nih.gov/entrez/eutils/esearch.fcgi";
const NCBI_EUTILS_SUMMARY_ENDPOINT: &str =
    "https://eutils.ncbi.nlm.nih.gov/entrez/eutils/esummary.fcgi";
const NCBI_MD5_MANIFEST: &str = "md5checksums.txt";
const MD5_VERIFY_THREADS: usize = 4;
const NCBI_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
const NCBI_DOWNLOAD_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, ValueEnum)]
pub enum AnnotationType {
    GenomeFasta,
    GenomeGtf,
    GenomeGff,
    GeneOntology,
    RnaFasta,
    CdsFasta,
    ProtFasta,
    SequenceReport,
    AssemblyReport,
    AssemblyStats,
}

impl AnnotationType {
    fn ftp_suffix(&self) -> &'static str {
        match self {
            Self::GenomeFasta => "_genomic.fna.gz",
            Self::GenomeGtf => "_genomic.gtf.gz",
            Self::GenomeGff => "_genomic.gff.gz",
            Self::GeneOntology => "_gene_ontology.gaf.gz",
            Self::RnaFasta => "_rna.fna.gz",
            Self::CdsFasta => "_cds_from_genomic.fna.gz",
            Self::ProtFasta => "_protein.faa.gz",
            Self::SequenceReport => "_sequence_report.jsonl",
            Self::AssemblyReport => "_assembly_report.txt",
            Self::AssemblyStats => "_assembly_stats.txt",
        }
    }
}

#[derive(Args, Debug)]
pub struct NcbiDownloadArgs {
    /// NCBI assembly accessions, e.g. GCF_004799145.2 GCF_002162155.1
    #[arg(value_name = "ACCESSION", num_args = 1.., help_heading = "Input Options")]
    pub accessions: Vec<String>,

    /// Output directory (default: ./<accession>)
    #[arg(short, long, value_name = "DIR", help_heading = "Input Options")]
    pub output: Option<PathBuf>,

    /// Number of assemblies to download concurrently
    #[arg(
        short = 'p',
        long = "multithreads",
        default_value_t = 2,
        help_heading = "Download Options"
    )]
    pub multithreads: usize,

    /// File types to download; specify more than once to select a subset
    #[arg(
        short = 'i',
        long = "include",
        value_enum,
        default_values_t = [
            AnnotationType::GenomeFasta,
            AnnotationType::GenomeGtf,
            AnnotationType::GenomeGff,
            AnnotationType::GeneOntology,
            AnnotationType::ProtFasta,
            AnnotationType::AssemblyReport,
            AnnotationType::AssemblyStats,
        ],
        help_heading = "Download Options"
    )]
    pub includes: Vec<AnnotationType>,

    /// NCBI API key for the assembly lookup request
    #[arg(
        long,
        value_name = "KEY",
        env = "NCBI_API_KEY",
        hide_env_values = true,
        help_heading = "Download Options"
    )]
    pub api_key: Option<String>,
}

impl NcbiDownloadArgs {
    pub fn output_dir_for(&self, accession: &str) -> PathBuf {
        match (&self.output, self.accessions.len()) {
            (Some(output), 1) => output.clone(),
            (Some(output), _) => output.join(accession),
            (None, _) => PathBuf::from(accession),
        }
    }
}

pub async fn run(args: &NcbiDownloadArgs, ui: Option<Arc<UiManager>>) -> Result<()> {
    if args.multithreads == 0 {
        return Err(anyhow!("--multithreads must be at least 1"));
    }
    for accession in &args.accessions {
        validate_accession(accession)?;
    }

    let client = Arc::new(
        Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(NCBI_DOWNLOAD_TIMEOUT)
            .build()
            .context("Failed to build NCBI FTP HTTP client")?,
    );

    let assemblies = stream::iter(args.accessions.iter().cloned().map(|accession| {
        let client = client.clone();
        let api_key = args.api_key.clone();
        let includes = args.includes.clone();
        let output_dir = args.output_dir_for(&accession);
        async move {
            prepare_assembly_downloads(
                &client,
                &accession,
                api_key.as_deref(),
                &includes,
                output_dir,
            )
            .await
        }
    }))
    .buffer_unordered(args.multithreads)
    .collect::<Vec<_>>()
    .await
    .into_iter()
    .collect::<Result<Vec<_>>>()?;

    if let Some(ui) = &ui {
        ui.set_total(
            assemblies
                .iter()
                .map(|assembly| assembly.downloads.len())
                .sum::<usize>() as u64,
        );
    }

    let results = stream::iter(assemblies.into_iter().map(|assembly| {
        let client = client.clone();
        let ui = ui.clone();
        async move { download_assembly(&client, assembly, ui.as_ref()).await }
    }))
    .buffer_unordered(args.multithreads)
    .collect::<Vec<_>>()
    .await;
    let failures = results
        .into_iter()
        .filter_map(Result::err)
        .collect::<Vec<_>>();
    if !failures.is_empty() {
        return Err(anyhow!(
            "{} NCBI assembly download task(s) failed: {}",
            failures.len(),
            failures[0]
        ));
    }

    info!("All NCBI assembly downloads completed successfully");
    Ok(())
}

struct AssemblyDownloads {
    accession: String,
    output_dir: PathBuf,
    downloads: Vec<NcbiDownload>,
    md5_manifest_url: Url,
}

struct NcbiDownload {
    filename: String,
    url: Url,
    total_size: Option<u64>,
}

async fn prepare_assembly_downloads(
    client: &Client,
    accession: &str,
    api_key: Option<&str>,
    includes: &[AnnotationType],
    output_dir: PathBuf,
) -> Result<AssemblyDownloads> {
    fs::create_dir_all(&output_dir)
        .with_context(|| format!("Failed to create output directory {}", output_dir.display()))?;
    info!("Locating NCBI FTP directory for {}...", accession);
    let ftp_directory = assembly_ftp_directory(client, accession, api_key).await?;
    let listing = fetch_directory_listing(client, &ftp_directory).await?;
    let downloads =
        inspect_downloads(client, ftp_file_urls(&ftp_directory, &listing, includes)?).await;
    let md5_manifest_url = ftp_directory
        .join(NCBI_MD5_MANIFEST)
        .context("Failed to construct the NCBI MD5 manifest URL")?;
    Ok(AssemblyDownloads {
        accession: accession.to_string(),
        output_dir,
        downloads,
        md5_manifest_url,
    })
}

async fn download_assembly(
    client: &Client,
    assembly: AssemblyDownloads,
    ui: Option<&Arc<UiManager>>,
) -> Result<()> {
    let md5_manifest_path = assembly.output_dir.join(NCBI_MD5_MANIFEST);
    info!(
        "[{}] Downloading {}...",
        assembly.accession, NCBI_MD5_MANIFEST
    );
    download_md5_manifest(client, &assembly.md5_manifest_url, &md5_manifest_path).await?;

    for download in &assembly.downloads {
        info!(
            "[{}] Downloading {}...",
            assembly.accession, download.filename
        );
        download_with_progress(
            client,
            &download,
            &assembly.output_dir.join(&download.filename),
            ui,
            &progress_label(&assembly.accession),
        )
        .await?;
    }

    let manifest_entries = parse_md5_manifest(&md5_manifest_path)?;
    let filenames = assembly
        .downloads
        .iter()
        .map(|download| download.filename.clone())
        .collect::<Vec<_>>();
    let entries = select_downloaded_md5_entries(manifest_entries, &filenames)?;
    info!(
        "[{}] Verifying MD5 for {} downloaded file(s)...",
        assembly.accession,
        entries.len()
    );
    let progress = ui.map(|ui| Arc::new(ui.multi_progress()));
    let (passed, failed) =
        verify_md5_entries(entries, &assembly.output_dir, MD5_VERIFY_THREADS, progress).await?;
    if failed > 0 {
        return Err(anyhow!(
            "MD5 verification failed for {} of {} downloaded file(s)",
            failed,
            passed + failed
        ));
    }
    info!(
        "[{}] MD5 verification passed for {} file(s)",
        assembly.accession, passed
    );
    info!("[{}] NCBI download completed", assembly.accession);
    Ok(())
}

async fn download_md5_manifest(client: &Client, url: &Url, output_path: &Path) -> Result<()> {
    let contents = client
        .get(url.clone())
        .send()
        .await
        .context("Failed to download NCBI MD5 manifest")?
        .error_for_status()
        .context("NCBI MD5 manifest request failed")?
        .bytes()
        .await
        .context("Failed to read NCBI MD5 manifest response")?;
    tokio::fs::write(output_path, contents)
        .await
        .with_context(|| {
            format!(
                "Failed to write NCBI MD5 manifest {}",
                output_path.display()
            )
        })
}

fn select_downloaded_md5_entries(
    manifest_entries: Vec<(String, String)>,
    filenames: &[String],
) -> Result<Vec<(String, String)>> {
    let checksums: HashMap<_, _> = manifest_entries
        .into_iter()
        .map(|(checksum, filename)| (filename.trim_start_matches("./").to_string(), checksum))
        .collect();

    filenames
        .iter()
        .map(|filename| {
            checksums
                .get(filename)
                .cloned()
                .map(|checksum| (checksum, filename.clone()))
                .ok_or_else(|| {
                    anyhow!(
                        "NCBI MD5 manifest does not contain a checksum for {}",
                        filename
                    )
                })
        })
        .collect()
}

fn validate_accession(accession: &str) -> Result<()> {
    let (prefix, identifier) = accession
        .split_once('_')
        .ok_or_else(|| anyhow!("Invalid assembly accession {accession:?}; expected GCA_<id>.<version> or GCF_<id>.<version>"))?;
    let (digits, version) = identifier
        .split_once('.')
        .ok_or_else(|| anyhow!("Invalid assembly accession {accession:?}; expected GCA_<id>.<version> or GCF_<id>.<version>"))?;

    if !matches!(prefix, "GCA" | "GCF")
        || digits.is_empty()
        || version.is_empty()
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
        || !version.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(anyhow!(
            "Invalid assembly accession {accession:?}; expected GCA_<id>.<version> or GCF_<id>.<version>"
        ));
    }

    Ok(())
}

async fn assembly_ftp_directory(
    client: &Client,
    accession: &str,
    api_key: Option<&str>,
) -> Result<Url> {
    let mut search_url = Url::parse(NCBI_EUTILS_SEARCH_ENDPOINT)
        .context("Failed to construct NCBI assembly search URL")?;
    {
        let mut query = search_url.query_pairs_mut();
        query.append_pair("db", "assembly");
        query.append_pair("term", accession);
        query.append_pair("retmode", "json");
        if let Some(api_key) = api_key {
            query.append_pair("api_key", api_key);
        }
    }
    let search = fetch_json(client, search_url, "NCBI assembly search").await?;
    let assembly_id = search
        .pointer("/esearchresult/idlist/0")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("NCBI could not find assembly {accession}"))?;

    let mut summary_url = Url::parse(NCBI_EUTILS_SUMMARY_ENDPOINT)
        .context("Failed to construct NCBI assembly summary URL")?;
    {
        let mut query = summary_url.query_pairs_mut();
        query.append_pair("db", "assembly");
        query.append_pair("id", assembly_id);
        query.append_pair("retmode", "json");
        if let Some(api_key) = api_key {
            query.append_pair("api_key", api_key);
        }
    }
    let summary = fetch_json(client, summary_url, "NCBI assembly summary").await?;
    let record = summary
        .pointer(&format!("/result/{assembly_id}"))
        .ok_or_else(|| anyhow!("NCBI assembly summary did not contain assembly {assembly_id}"))?;
    let ftp_path = record
        .get("ftppath_refseq")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .or_else(|| {
            record
                .get("ftppath_genbank")
                .and_then(Value::as_str)
                .filter(|path| !path.is_empty())
        })
        .ok_or_else(|| {
            anyhow!(
                "NCBI assembly {} does not provide an FTP directory",
                accession
            )
        })?;

    let mut ftp_url = Url::parse(ftp_path)
        .with_context(|| format!("Invalid NCBI FTP directory URL {ftp_path:?}"))?;
    ftp_url
        .set_scheme("https")
        .map_err(|_| anyhow!("Failed to convert NCBI FTP directory URL to HTTPS"))?;
    if !ftp_url.path().ends_with('/') {
        let path = format!("{}/", ftp_url.path());
        ftp_url.set_path(&path);
    }
    Ok(ftp_url)
}

async fn fetch_json(client: &Client, url: Url, request_name: &str) -> Result<Value> {
    client
        .get(url)
        .send()
        .await
        .with_context(|| format!("{request_name} request failed"))?
        .error_for_status()
        .with_context(|| format!("{request_name} request returned an error"))?
        .json()
        .await
        .with_context(|| format!("Failed to parse {request_name} response"))
}

async fn fetch_directory_listing(client: &Client, ftp_directory: &Url) -> Result<String> {
    client
        .get(ftp_directory.clone())
        .send()
        .await
        .context("Failed to retrieve NCBI assembly FTP directory listing")?
        .error_for_status()
        .context("NCBI assembly FTP directory listing returned an error")?
        .text()
        .await
        .context("Failed to read NCBI assembly FTP directory listing")
}

fn ftp_file_urls(
    ftp_directory: &Url,
    listing: &str,
    includes: &[AnnotationType],
) -> Result<Vec<(String, Url)>> {
    let filenames: Vec<_> = listing
        .split("href=\"")
        .skip(1)
        .filter_map(|entry| entry.split_once('"').map(|(filename, _)| filename))
        .collect();
    let mut files = Vec::new();

    for annotation_type in includes {
        let suffix = annotation_type.ftp_suffix();
        let Some(filename) = filenames
            .iter()
            .copied()
            .filter(|filename| {
                !filename.contains('/') && filename.ends_with(suffix) && !filename.contains('?')
            })
            .min_by_key(|filename| filename.len())
        else {
            warn!(
                "NCBI FTP directory does not contain a file ending in {}; skipping it",
                suffix
            );
            continue;
        };
        if files.iter().any(|(existing, _)| existing == filename) {
            continue;
        }
        let url = ftp_directory
            .join(filename)
            .with_context(|| format!("Failed to construct NCBI FTP URL for {filename}"))?;
        files.push((filename.to_string(), url));
    }

    if files.is_empty() {
        return Err(anyhow!(
            "NCBI FTP directory does not contain any of the requested files"
        ));
    }

    Ok(files)
}

async fn inspect_downloads(client: &Client, files: Vec<(String, Url)>) -> Vec<NcbiDownload> {
    let mut downloads = Vec::with_capacity(files.len());
    for (filename, url) in files {
        let total_size = remote_file_size(client, url.clone()).await;
        downloads.push(NcbiDownload {
            filename,
            url,
            total_size,
        });
    }
    downloads
}

async fn remote_file_size(client: &Client, url: Url) -> Option<u64> {
    let range_response = client
        .get(url.clone())
        .header(RANGE, "bytes=0-0")
        .send()
        .await
        .ok()?;
    if range_response.status().is_success() {
        if let Some(total_size) = range_response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(content_range_total_size)
        {
            return Some(total_size);
        }
    }

    match client.head(url).send().await {
        Ok(response) if response.status().is_success() => response.content_length(),
        _ => None,
    }
}

fn content_range_total_size(value: &str) -> Option<u64> {
    value.rsplit_once('/')?.1.parse().ok()
}

fn progress_label(filename: &str) -> String {
    filename.chars().take(10).collect()
}

async fn download_with_progress(
    client: &Client,
    download: &NcbiDownload,
    destination: &Path,
    ui: Option<&Arc<UiManager>>,
    progress_id: &str,
) -> Result<()> {
    let started_at = std::time::Instant::now();
    let total_size = download.total_size.unwrap_or(0);
    let counter = ui.map(|ui| ui.register(&download.filename, total_size));
    let file_progress = ui.map(|ui| ui.transfer_bar(progress_id, total_size));

    let mut attempt = 1;
    let result = loop {
        if attempt > 1 {
            warn!(
                "Retrying NCBI download for {} (attempt {}/{})",
                download.url, attempt, NCBI_DOWNLOAD_ATTEMPTS
            );
            tokio::time::sleep(Duration::from_secs(attempt as u64)).await;
            if let Some(file_progress) = &file_progress {
                file_progress.set_position(0);
            }
            if let Some(counter) = &counter {
                counter.store(0, Ordering::Relaxed);
            }
        }

        match download_file_once(
            client,
            download,
            destination,
            file_progress.as_ref(),
            counter.as_ref(),
        )
        .await
        {
            Ok(()) => break Ok(()),
            Err(error) if attempt < NCBI_DOWNLOAD_ATTEMPTS => {
                warn!(
                    "NCBI download attempt {}/{} failed for {}: {:#}",
                    attempt, NCBI_DOWNLOAD_ATTEMPTS, download.url, error
                );
                attempt += 1;
            }
            Err(error) => break Err(error),
        }
    };

    if let Some(file_progress) = file_progress {
        file_progress.finish_and_clear();
    }
    if let Some(ui) = ui {
        ui.unregister(&download.filename);
        match &result {
            Ok(()) => ui.complete(CompletedInfo {
                id: download.filename.clone(),
                total_bytes: total_size,
                elapsed_secs: started_at.elapsed().as_secs_f64(),
                avg_speed_bps: total_size as f64 / started_at.elapsed().as_secs_f64().max(0.001),
            }),
            Err(_) => ui.fail(&download.filename),
        }
    }
    result
}

async fn download_file_once(
    client: &Client,
    download: &NcbiDownload,
    destination: &Path,
    file_progress: Option<&indicatif::ProgressBar>,
    counter: Option<&Arc<std::sync::atomic::AtomicU64>>,
) -> Result<()> {
    let response = client
        .get(download.url.clone())
        .send()
        .await
        .with_context(|| format!("NCBI FTP download request failed for {}", download.url))?;
    let status = response.status();
    if !status.is_success() {
        return Err(anyhow!(
            "NCBI FTP download request returned HTTP {} for {}",
            status,
            download.url
        ));
    }

    let mut file = fs::File::create(destination)
        .with_context(|| format!("Failed to create output file {}", destination.display()))?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| {
            format!(
                "Failed while receiving NCBI FTP response body from {}",
                download.url
            )
        })?;
        std::io::Write::write_all(&mut file, &chunk)
            .with_context(|| format!("Failed to write output file {}", destination.display()))?;
        if let Some(file_progress) = file_progress {
            file_progress.inc(chunk.len() as u64);
        }
        if let Some(counter) = counter {
            counter.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        }
    }
    std::io::Write::flush(&mut file)
        .with_context(|| format!("Failed to flush output file {}", destination.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: &str = r#"
        <a href="GCF_902201215.1_ALMONDv2_cds_from_genomic.fna.gz">CDS</a>
        <a href="GCF_902201215.1_ALMONDv2_assembly_report.txt">Assembly report</a>
        <a href="GCF_902201215.1_ALMONDv2_assembly_stats.txt">Assembly stats</a>
        <a href="GCF_902201215.1_ALMONDv2_gene_ontology.gaf.gz">GO</a>
        <a href="GCF_902201215.1_ALMONDv2_genomic.fna.gz">Genome</a>
        <a href="GCF_902201215.1_ALMONDv2_genomic.gff.gz">GFF</a>
        <a href="GCF_902201215.1_ALMONDv2_genomic.gtf.gz">GTF</a>
        <a href="GCF_902201215.1_ALMONDv2_protein.faa.gz">Protein</a>
        <a href="GCF_902201215.1_ALMONDv2_rna.fna.gz">RNA</a>
        <a href="GCF_902201215.1_ALMONDv2_sequence_report.jsonl">Report</a>
    "#;

    fn example_args() -> NcbiDownloadArgs {
        NcbiDownloadArgs {
            accessions: vec!["GCF_004799145.2".to_string()],
            output: None,
            multithreads: 2,
            includes: vec![
                AnnotationType::GenomeFasta,
                AnnotationType::GenomeGtf,
                AnnotationType::GenomeGff,
                AnnotationType::GeneOntology,
                AnnotationType::ProtFasta,
                AnnotationType::AssemblyReport,
                AnnotationType::AssemblyStats,
            ],
            api_key: None,
        }
    }

    #[test]
    fn validates_refseq_and_genbank_assembly_accessions() {
        assert!(validate_accession("GCF_004799145.2").is_ok());
        assert!(validate_accession("GCA_000001405.29").is_ok());
        assert!(validate_accession("SRR123456").is_err());
        assert!(validate_accession("GCF_004799145").is_err());
        assert!(validate_accession("GCF_bad.2").is_err());
    }

    #[test]
    fn default_output_directory_is_accession() {
        assert_eq!(
            example_args().output_dir_for("GCF_004799145.2"),
            PathBuf::from("GCF_004799145.2")
        );
    }

    #[test]
    fn multiple_accessions_use_separate_output_subdirectories() {
        let mut args = example_args();
        args.accessions.push("GCF_002162155.1".to_string());
        args.output = Some(PathBuf::from("genomes"));

        assert_eq!(
            args.output_dir_for("GCF_002162155.1"),
            PathBuf::from("genomes/GCF_002162155.1")
        );
    }

    #[test]
    fn selects_the_default_annotation_and_assembly_files() {
        let directory = Url::parse("https://ftp.ncbi.nlm.nih.gov/genomes/all/GCF/example/")
            .expect("valid directory URL");
        let files = ftp_file_urls(&directory, LISTING, &example_args().includes)
            .expect("all default files found");
        let filenames: Vec<_> = files
            .iter()
            .map(|(filename, _)| filename.as_str())
            .collect();

        assert_eq!(
            filenames,
            vec![
                "GCF_902201215.1_ALMONDv2_genomic.fna.gz",
                "GCF_902201215.1_ALMONDv2_genomic.gtf.gz",
                "GCF_902201215.1_ALMONDv2_genomic.gff.gz",
                "GCF_902201215.1_ALMONDv2_gene_ontology.gaf.gz",
                "GCF_902201215.1_ALMONDv2_protein.faa.gz",
                "GCF_902201215.1_ALMONDv2_assembly_report.txt",
                "GCF_902201215.1_ALMONDv2_assembly_stats.txt",
            ]
        );
    }

    #[test]
    fn supports_selecting_a_subset_of_ftp_files() {
        let directory = Url::parse("https://ftp.ncbi.nlm.nih.gov/genomes/all/GCF/example/")
            .expect("valid directory URL");
        let files = ftp_file_urls(
            &directory,
            LISTING,
            &[AnnotationType::RnaFasta, AnnotationType::SequenceReport],
        )
        .expect("selected files found");

        assert_eq!(files.len(), 2);
        assert!(files[0].0.ends_with("_rna.fna.gz"));
        assert!(files[1].0.ends_with("_sequence_report.jsonl"));
    }

    #[test]
    fn rejects_an_assembly_with_no_requested_ftp_files() {
        let directory = Url::parse("https://ftp.ncbi.nlm.nih.gov/genomes/all/GCF/example/")
            .expect("valid directory URL");
        assert!(ftp_file_urls(&directory, "", &[AnnotationType::GenomeFasta]).is_err());
    }

    #[test]
    fn skips_unavailable_requested_ftp_files() {
        let directory = Url::parse("https://ftp.ncbi.nlm.nih.gov/genomes/all/GCF/example/")
            .expect("valid directory URL");
        let files = ftp_file_urls(
            &directory,
            LISTING,
            &[AnnotationType::GenomeFasta, AnnotationType::GeneOntology],
        )
        .expect("available files should still be returned");

        assert_eq!(files.len(), 2);

        let files = ftp_file_urls(
            &directory,
            "<a href=\"GCF_example_genomic.fna.gz\">genome</a>",
            &[AnnotationType::GenomeFasta, AnnotationType::GeneOntology],
        )
        .expect("the available genome FASTA should be kept");
        assert_eq!(files.len(), 1);
        assert!(files[0].0.ends_with("_genomic.fna.gz"));
    }

    #[test]
    fn selects_only_downloaded_files_from_the_ncbi_md5_manifest() {
        let entries = select_downloaded_md5_entries(
            vec![
                (
                    "0123456789abcdef0123456789abcdef".to_string(),
                    "./GCF_example_genomic.fna.gz".to_string(),
                ),
                (
                    "fedcba9876543210fedcba9876543210".to_string(),
                    "./unrequested.gbff.gz".to_string(),
                ),
            ],
            &["GCF_example_genomic.fna.gz".to_string()],
        )
        .expect("selected file checksum should exist");

        assert_eq!(
            entries,
            vec![(
                "0123456789abcdef0123456789abcdef".to_string(),
                "GCF_example_genomic.fna.gz".to_string(),
            )]
        );
    }

    #[test]
    fn rejects_downloaded_files_absent_from_the_ncbi_md5_manifest() {
        let error =
            select_downloaded_md5_entries(Vec::new(), &["GCF_example_genomic.fna.gz".to_string()])
                .expect_err("missing checksums must fail validation");

        assert!(error.to_string().contains("does not contain a checksum"));
    }

    #[test]
    fn parses_total_size_from_http_content_range() {
        assert_eq!(
            content_range_total_size("bytes 0-0/69988148"),
            Some(69_988_148)
        );
        assert_eq!(content_range_total_size("bytes 0-0/*"), None);
    }

    #[test]
    fn progress_labels_use_the_first_ten_characters() {
        assert_eq!(
            progress_label("GCF_902201215.1_ALMONDv2_genomic.fna.gz"),
            "GCF_902201"
        );
    }
}
