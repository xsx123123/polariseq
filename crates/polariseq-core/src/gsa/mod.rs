//! GSA (Genome Sequence Archive, NGDC CNCB) data-source resolver.
//!
//! Resolves CRA (project), CRR (run) accessions or GSA browse URLs into a
//! concrete file list with FTP/HTTPS download URLs. File discovery and
//! downloads are FTP-first: `ftp://download.cncb.ac.cn` supports anonymous
//! LIST/RETR, which keeps the whole pipeline free of the NGDC web anti-bot
//! challenge and the AWS-S3 bulk downloader.
//!
//! Verified facts (2026-10, see docs/info/GSA数据下载功能实施手册.md):
//! - Generations: `gsa`, `gsa2`..`gsa6` (no `gsa1`); a CRA lives in exactly one.
//! - `LIST /<gen>/<CRA>/` enumerates CRR run directories (new layout) or
//!   SAMC sample directories (legacy `gsa` layout, files sit under CRX).
//! - `<CRA>/md5sum.txt` maps `MD5\t/CRA/CRR/<filename>` (uppercase hex).
//! - CRR→CRA ownership: the server-rendered page
//!   `https://ngdc.cncb.ac.cn/gsa/search?searchTerm=<CRR>` embeds a
//!   `browse/<CRA>/<CRR>` link and needs no anti-bot cookie.

use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use reqwest::header;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info, warn};

/// FTP host serving anonymous GSA data.
pub const GSA_FTP_HOST: &str = "download.cncb.ac.cn";
/// Server-rendered search endpoint that resolves accessions without the
/// browser anti-bot challenge.
const GSA_SEARCH_URL: &str = "https://ngdc.cncb.ac.cn/gsa/search?searchTerm=";
/// Retry policy for resolver HTTP calls (mirrors SraUtils::get_metadata).
const MAX_RETRIES: u32 = 5;
/// Remote listing probe timeout.
const LIST_TIMEOUT: u64 = 60;

/// One downloadable GSA file.
#[derive(Debug, Clone)]
pub struct GsaFile {
    pub filename: String,
    pub size: u64,
    pub https_url: String,
    pub ftp_url: String,
    pub md5: Option<String>,
}

/// A resolved GSA run with its files.
#[derive(Debug, Clone)]
pub struct GsaRun {
    pub cra: String,
    pub crr: String,
    pub files: Vec<GsaFile>,
}

/// A single wget download task (FTP-first).
#[derive(Debug, Clone)]
pub struct GsaDownloadTask {
    pub crr: String,
    pub filename: String,
    pub url: String,
    pub total_size: u64,
    pub md5: Option<String>,
}

/// Parsed user input for GSA downloads.
#[derive(Debug, Clone, PartialEq)]
pub enum GsaInput {
    /// CRA accession — downloads every run under it.
    Cra(String),
    /// CRR accession(s) — resolved to their owning CRA.
    Crr(String),
}

/// Parse raw user input into a [`GsaInput`].
///
/// Accepts:
/// - CRA accessions: `CRA017017`
/// - CRR accessions: `CRR1235475`
/// - Browse URLs ending in CRA or CRA/CRR:
///   `https://ngdc.cncb.ac.cn/gsa/browse/CRA017017[/CRR1235475]`
pub fn parse_gsa_input(raw: &str) -> Result<GsaInput> {
    let s = raw.trim().trim_end_matches('/');
    if let Some(pos) = s.find("/browse/") {
        // Browse URL: take the trailing accession segments.
        let tail = &s[pos + "/browse/".len()..];
        let segs: Vec<&str> = tail.split('/').filter(|s| !s.is_empty()).collect();
        return match segs.as_slice() {
            [cra] => parse_accession(cra),
            [cra, crr] => {
                // URL must pair a CRA with a CRR; catch swapped/mismatched ids.
                match (parse_accession(cra)?, parse_accession(crr)?) {
                    (GsaInput::Cra(_), crr_in @ GsaInput::Crr(_)) => Ok(crr_in),
                    _ => bail!("URL mix inconsistency: expected <CRA>/<CRR> in {raw}"),
                }
            }
            _ => bail!("Unsupported GSA browse URL: {raw}"),
        };
    }
    parse_accession(s)
}

fn parse_accession(s: &str) -> Result<GsaInput> {
    let upper = s.to_uppercase();
    // Accessions carry at least 3 digits after the prefix (CRA000000+).
    let digits_valid = |rest: &str| rest.len() >= 3 && rest.chars().all(|c| c.is_ascii_digit());
    if upper.starts_with("CRA") && digits_valid(&upper[3..]) {
        Ok(GsaInput::Cra(upper))
    } else if upper.starts_with("CRR") && digits_valid(&upper[3..]) {
        Ok(GsaInput::Crr(upper))
    } else {
        Err(anyhow!(
            "Unsupported GSA input '{s}': expected a CRA/CRR accession or a browse URL"
        ))
    }
}

/// Resolve a list of user inputs into concrete download tasks.
pub async fn resolve_inputs(inputs: &[GsaInput]) -> Result<Vec<GsaRun>> {
    let mut runs = Vec::new();
    for input in inputs {
        match input {
            GsaInput::Cra(cra) => runs.extend(resolve_cra(cra).await?),
            GsaInput::Crr(crr) => runs.push(resolve_crr(crr).await?),
        }
    }
    Ok(runs)
}

/// Resolve a CRA into all its runs by listing the FTP directory.
async fn resolve_cra(cra: &str) -> Result<Vec<GsaRun>> {
    info!("Resolving GSA CRA {} via FTP listing...", cra);
    let gen = find_generation(cra).await?;
    let base = format!("ftp://{GSA_FTP_HOST}/{gen}/{cra}");
    let listing = ftp_list(&format!("{base}/")).await?;
    let md5_map = fetch_cra_md5_map(&gen, cra).await;

    let mut runs = Vec::new();
    for line in listing.lines() {
        // New layout: CRR directories. Legacy `gsa` layout nests SAMC/CRX.
        let dir = line.split_whitespace().last().unwrap_or("");
        if line.starts_with('d') && dir.starts_with("CRR") {
            let run_dir = format!("{base}/{dir}/");
            let files = list_run_files(&gen, cra, dir, &run_dir, &md5_map).await?;
            runs.push(GsaRun {
                cra: cra.to_string(),
                crr: dir.to_string(),
                files,
            });
        }
    }
    if runs.is_empty() {
        bail!(
            "No CRR run directories found for CRA {cra} under ftp://{GSA_FTP_HOST}/{gen}/{cra}"
        );
    }
    info!(
        "Resolved CRA {} (generation {}): {} run(s)",
        cra,
        gen,
        runs.len()
    );
    Ok(runs)
}

/// Resolve a single CRR by locating its owning CRA through the server-rendered
/// search page, then listing the run's FTP directory.
async fn resolve_crr(crr: &str) -> Result<GsaRun> {
    info!("Resolving GSA CRR {} ...", crr);
    let cra = find_cra_for_crr(crr).await?;
    debug!("CRR {} belongs to CRA {}", crr, cra);
    let gen = find_generation(&cra).await?;
    let run_dir = format!("ftp://{GSA_FTP_HOST}/{gen}/{cra}/{crr}/");
    let md5_map = fetch_cra_md5_map(&gen, &cra).await;
    let files = list_run_files(&gen, &cra, crr, &run_dir, &md5_map).await?;
    Ok(GsaRun {
        cra,
        crr: crr.to_string(),
        files,
    })
}

/// Find which generation directory (`gsa`, `gsa2`..) hosts the CRA.
async fn find_generation(cra: &str) -> Result<String> {
    const GENERATIONS: [&str; 6] = ["gsa", "gsa2", "gsa3", "gsa4", "gsa5", "gsa6"];
    let mut last_err: Option<anyhow::Error> = None;
    for gen in GENERATIONS {
        let url = format!("ftp://{GSA_FTP_HOST}/{gen}/{cra}/");
        match ftp_list(&url).await {
            Ok(_) => return Ok(gen.to_string()),
            Err(e) => {
                debug!("CRA {} not under {}: {}", cra, gen, e);
                last_err = Some(e);
            }
        }
    }
    Err(anyhow!(
        "CRA {} not found under any GSA generation directory (gsa, gsa2..gsa6): {}",
        cra,
        last_err.map(|e| e.to_string()).unwrap_or_default()
    ))
}

/// Extract the owning CRA for a CRR from the search result HTML.
///
/// The page embeds links like `browse/CRA017017/CRR1235475`; we pick the CRA
/// whose paired CRR matches the query.
async fn find_cra_for_crr(crr: &str) -> Result<String> {
    let body = http_get_with_retry(&format!("{GSA_SEARCH_URL}{crr}")).await?;
    let re = Regex::new(&format!(
        r#"browse/(CRA\d+)/{crr}"#
    ))
    .context("Failed to compile CRR-to-CRA regex")?;
    if let Some(cap) = re.captures(&body) {
        return Ok(cap[1].to_string());
    }
    // Fall back to any CRA link in the run table context.
    let re_any = Regex::new(r#"browse/(CRA\d+)/"#).context("Failed to compile CRA regex")?;
    if let Some(cap) = re_any.captures(&body) {
        warn!(
            "Exact CRR{} pair not found in search page; using first CRA link {}",
            crr,
            &cap[1]
        );
        return Ok(cap[1].to_string());
    }
    bail!(
        "Could not locate CRR {crr} in GSA search results (it may not exist or is not public)"
    )
}

/// List the files of one run directory and attach md5s from the CRA manifest.
async fn list_run_files(
    gen: &str,
    cra: &str,
    crr: &str,
    run_dir: &str,
    md5_map: &Option<std::collections::HashMap<String, String>>,
) -> Result<Vec<GsaFile>> {
    let listing = ftp_list(run_dir).await?;
    let https_base = format!("https://download.cncb.ac.cn/{gen}/{cra}/{crr}");
    let ftp_base = run_dir.trim_end_matches('/');

    let mut files = Vec::new();
    for line in listing.lines() {
        if !line.starts_with('-') {
            continue;
        }
        let filename = line.split_whitespace().last().unwrap_or("").to_string();
        if filename.is_empty() {
            continue;
        }
        let size = parse_ftp_size(line);
        // md5sum.txt keys are "/CRA/CRR/<filename>".
        let md5 = md5_map
            .as_ref()
            .and_then(|m| m.get(&format!("/{cra}/{crr}/{filename}")))
            .cloned();
        files.push(GsaFile {
            https_url: format!("{https_base}/{filename}"),
            ftp_url: format!("{ftp_base}/{filename}"),
            filename,
            size,
            md5,
        });
    }
    if files.is_empty() {
        bail!("Run directory {run_dir} is empty");
    }
    Ok(files)
}

/// Parse the byte size out of a UNIX-style FTP LIST line.
fn parse_ftp_size(line: &str) -> u64 {
    line.split_whitespace()
        .nth(4)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0)
}

/// Download `<CRA>/md5sum.txt` and build a filename→md5 map.
///
/// Failures are non-fatal: the manifest is an optimization, per-file md5s can
/// also come from the run-level flow. Uppercase hex is normalized to lowercase.
async fn fetch_cra_md5_map(
    gen: &str,
    cra: &str,
) -> Option<std::collections::HashMap<String, String>> {
    let url = format!("https://download.cncb.ac.cn/{gen}/{cra}/md5sum.txt");
    match http_get_with_retry(&url).await {
        Ok(text) => {
            let mut map = std::collections::HashMap::new();
            for line in text.lines() {
                let mut parts = line.splitn(2, '\t');
                let (Some(md5), Some(path)) = (parts.next(), parts.next()) else {
                    continue;
                };
                map.insert(path.trim().to_string(), md5.trim().to_lowercase());
            }
            if map.is_empty() {
                warn!("md5sum.txt for CRA {} is empty", cra);
                None
            } else {
                info!(
                    "Loaded {} md5 entries from {}/md5sum.txt",
                    map.len(),
                    cra
                );
                Some(map)
            }
        }
        Err(e) => {
            warn!("md5sum.txt unavailable for CRA {}: {}", cra, e);
            None
        }
    }
}

/// List a directory over anonymous FTP via `curl` (FTP LIST).
///
/// Uses curl instead of spawning `wget` because only a short listing is needed
/// and curl is a hard dependency of the FTP download path anyway.
async fn ftp_list(url: &str) -> Result<String> {
    let output = tokio::process::Command::new("curl")
        .args(["-sS", "--max-time", &LIST_TIMEOUT.to_string(), url])
        .output()
        .await
        .context("Failed to spawn curl for FTP listing (is curl installed?)")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("FTP listing failed for {}: {}", url, stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// HTTP GET with exponential-backoff retries (pattern from
/// `SraUtils::get_metadata`), honoring Retry-After when present.
async fn http_get_with_retry(url: &str) -> Result<String> {
    // NGDC closes bare/no-UA TLS connections before responding; a browser-like
    // User-Agent keeps the server-rendered search page reachable.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(LIST_TIMEOUT))
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36")
        .build()
        .context("Failed to build HTTP client")?;

    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match client.get(url).send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    return resp
                        .text()
                        .await
                        .with_context(|| format!("Failed to read response body of {url}"));
                }
                if attempt >= MAX_RETRIES {
                    return Err(anyhow!("HTTP Error {} for {}", resp.status(), url));
                }
                let status = resp.status();
                let retry_after_secs = resp
                    .headers()
                    .get(header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok());
                let wait_secs = retry_after_secs
                    .unwrap_or_else(|| std::cmp::min(60, 2_u64.saturating_pow(attempt)))
                    + 1;
                warn!(
                    "[Network] {} returned {}, retrying in {}s ({}/{})...",
                    url, status, wait_secs, attempt, MAX_RETRIES
                );
                sleep(Duration::from_secs(wait_secs.max(1))).await;
            }
            Err(e) => {
                if attempt >= MAX_RETRIES {
                    return Err(anyhow!(
                        "Failed to connect to {url} after {MAX_RETRIES} attempts: {e}"
                    ));
                }
                let wait_secs = std::cmp::min(60, 2_u64.saturating_pow(attempt));
                warn!(
                    "[Network] Connection failed: {}. Retrying in {}s ({}/{})...",
                    e, wait_secs, attempt, MAX_RETRIES
                );
                sleep(Duration::from_secs(wait_secs.max(1))).await;
            }
        }
    }
}

/// Convert resolved runs into wget download tasks (FTP-first).
pub fn build_download_tasks(runs: &[GsaRun], exclude_xml: bool) -> Vec<GsaDownloadTask> {
    let mut tasks = Vec::new();
    for run in runs {
        for file in &run.files {
            if exclude_xml && file.filename.ends_with(".xml") {
                debug!("Skipping metadata file {}", file.filename);
                continue;
            }
            tasks.push(GsaDownloadTask {
                crr: run.crr.clone(),
                filename: file.filename.clone(),
                url: file.ftp_url.clone(),
                total_size: file.size,
                md5: file.md5.clone(),
            });
        }
    }
    tasks
}

/// Output subdirectory for a run: `<output>/<CRR>`.
pub fn run_output_dir(base: &Path, crr: &str) -> PathBuf {
    base.join(crr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cra() {
        assert_eq!(
            parse_gsa_input("CRA017017").unwrap(),
            GsaInput::Cra("CRA017017".to_string())
        );
    }

    #[test]
    fn parse_crr_case_insensitive() {
        assert_eq!(
            parse_gsa_input("crr1235475").unwrap(),
            GsaInput::Crr("CRR1235475".to_string())
        );
    }

    #[test]
    fn parse_browse_url_crr() {
        assert_eq!(
            parse_gsa_input("https://ngdc.cncb.ac.cn/gsa/browse/CRA017017/CRR1235475").unwrap(),
            GsaInput::Crr("CRR1235475".to_string())
        );
    }

    #[test]
    fn parse_browse_url_cra() {
        assert_eq!(
            parse_gsa_input("https://ngdc.cncb.ac.cn/gsa/browse/CRA017017").unwrap(),
            GsaInput::Cra("CRA017017".to_string())
        );
    }

    #[test]
    fn reject_bad_accession() {
        assert!(parse_gsa_input("SRX10535559").is_err());
        assert!(parse_gsa_input("CRA00").is_err());
        assert!(parse_gsa_input("hello").is_err());
    }

    #[test]
    fn ftp_size_parsing() {
        assert_eq!(
            parse_ftp_size("-rw-r--r--    1 1231     1040     2530594764 Jun 26  2025 CRR1235475_f1.fq.gz"),
            2_530_594_764
        );
        assert_eq!(parse_ftp_size("garbage"), 0);
    }

    #[test]
    fn build_tasks_skips_xml() {
        let run = GsaRun {
            cra: "CRA017017".to_string(),
            crr: "CRR1235475".to_string(),
            files: vec![
                GsaFile {
                    filename: "CRR1235475_f1.fq.gz".to_string(),
                    size: 10,
                    https_url: "https://x/1".to_string(),
                    ftp_url: "ftp://x/1".to_string(),
                    md5: Some("a".to_string()),
                },
                GsaFile {
                    filename: "CRR1235475_sta.xml".to_string(),
                    size: 5,
                    https_url: "https://x/2".to_string(),
                    ftp_url: "ftp://x/2".to_string(),
                    md5: None,
                },
            ],
        };
        assert_eq!(build_download_tasks(&[run.clone()], false).len(), 2);
        let tasks = build_download_tasks(&[run], true);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].filename, "CRR1235475_f1.fq.gz");
    }
}
