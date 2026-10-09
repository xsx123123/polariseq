# Polariseq 架构梳理报告 —— GSA（NGDC）数据源接入依据

> 生成日期：2026-10-08 · 只读调查，未修改任何业务代码
> 调查范围：crates/polariseq-core、crates/polariseq-cli、crates/polariseq-gui（src-tauri）、README.md、polariseq.yaml、Cargo manifests

## 0. Workspace 总览

| Crate | 角色 | 二进制名 |
|---|---|---|
| `crates/polariseq-core` | 全部业务逻辑库（下载/校验/公共数据/依赖管理） | 无（lib） |
| `crates/polariseq-cli` | clap CLI，含终端 UI 与 HTTP Progress 服务 | `polariseq` |
| `crates/polariseq-gui/src-tauri` | Tauri 2.0 桌面端，直接依赖 core | `polariseq` |

- Cargo workspace 定义：`Cargo.toml:2-7`
- CLI 依赖 core 并开启 `cli` feature：`crates/polariseq-cli/Cargo.toml:12`
- GUI 依赖 core：`crates/polariseq-gui/src-tauri/Cargo.toml:12`

---

## 1. 模块地图（polariseq-core）

```
polariseq-core/src
├── lib.rs            ← 顶层共享类型 + ENA 元数据管道（根模块）
│   ├─ Config / SoftwarePaths          lib.rs:25-37（YAML 配置，serde）
│   ├─ EnaRecord                       lib.rs:39-94（ENA filereport TSV 行）
│   ├─ ProcessedRecord                 lib.rs:96-108（拆分后的单/双端下载条目：url+md5+size）
│   ├─ DownloadMethod{Ftp,Aws}         lib.rs:110-115
│   ├─ DownloadOptions / UploadOptions lib.rs:117-185
│   ├─ load_config()                   lib.rs:187-197（serde_yaml 反序列化）
│   ├─ fetch_ena_data()                lib.rs:199-224（ENA portal API → Vec<EnaRecord>）
│   ├─ read_tsv_data()                 lib.rs:226-239（本地 TSV 输入）
│   ├─ RegexFilters{should_include}    lib.rs:241-321（sample/run 正则过滤）
│   ├─ process_records()               lib.rs:323-390（EnaRecord→ProcessedRecord，拆 ';' 分隔的多文件字段）
│   ├─ compress_fastq_files()            lib.rs:394-467（gzp 并行 gzip + 进度回调）
│   └─ generate_md5sum_file[_at]()     lib.rs:497-523（md5sum 兼容清单生成）
├── aws_s3.rs          ← HTTP Range 多线程断点续传下载器（对任意支持 Range 的 https 通用）
│   ├─ SraMetadata{s3_uri,http_url,md5,size}   aws_s3.rs:28-33（下载器唯一输入描述）
│   ├─ PauseToken                            aws_s3.rs:38-65（GUI 暂停令牌）
│   ├─ SraUtils::get_metadata()               aws_s3.rs:129-200（NCBI efetch XML→SraMetadata，含全局限速+重试）
│   ├─ resolve_urls/parse_sra_xml            aws_s3.rs:207-284（NCBI 专属，仅被 get_metadata 用）
│   ├─ ResumableDownloader                    aws_s3.rs:286-728（核心下载器，详见 §A）
│   └─ download_chunk_http()                  aws_s3.rs:730-876（单 chunk Range 下载+偏移续传+429 退避）
├── ftp.rs             ← FTP 模式：外部 `wget -c` 子进程 + 下载后 MD5 校验
│   ├─ Protocol::Ftp    ftp.rs:15-17
│   ├─ process_downloads()  ftp.rs:19-218（并发=Semaphore；命令构造 ftp.rs:74-78；下载后 verify_md5 ftp.rs:167-184）
│   └─ verify_md5()     ftp.rs:220-236
├── md5.rs             ← md5 库函数（generate/verify 均在此，CLI 子命令只是薄壳）
│   ├─ compute_md5[_with_progress]()  md5.rs:17-41
│   ├─ parse_md5_manifest()           md5.rs:92-121（解析 "<md5>  <filename>" 清单）
│   ├─ collect_files()               md5.rs:139-144（递归收集，跳隐藏/polariseq_md5_*.log）
│   ├─ generate_md5_manifest()       md5.rs:184-262（多线程生成清单）
│   ├─ verify_md5_manifest()         md5.rs:292-335（清单→逐文件校验）
│   └─ verify_md5_entries()          md5.rs:342-418（按给定 (md5,filename) 集校验 ← ncbi-download 内建校验走这里）
├── observer.rs        ← DownloadObserver trait（core↔CLI UI 解耦桥）
│   ├─ CompletedInfo                observer.rs:13-18
│   └─ trait DownloadObserver{set_total,register,unregister,complete,fail}  observer.rs:25-43
├── progress.rs        ← indicatif 进度条样式（transfer/verify/spinner/phase）progress.rs:3-41
├── progress_store.rs  ← RunProgress/RunStage/StageProgress 共享状态（GUI 事件 + HTTP Progress API 的数据源）
│   ├─ ProgressStore = Arc<RwLock<HashMap<String,RunProgress>>>  progress_store.rs:6
│   └─ RunProgress{recalculate_overall} StageProgress::update    progress_store.rs:12-74
├── public_data/       ← YAML 可配置 S3 公共数据库下载
│   ├─ config.rs  PublicDatabase/ValidateConfig/DatabaseType       config.rs:4-52
│   ├─ downloader.rs  PublicDataDownloader（复用 ResumableDownloader）downloader.rs:84-607
│   ├─ s3.rs  parse_s3_url/s3_url_to_https/should_download_key      s3.rs:13-54
│   └─ validator.rs  blastdbcmd 卷校验
├── deps/mod.rs        ← sra-tools 自动下载安装（含进度回调 DepProgressCallback）deps/mod.rs:29-51
└── upload/mod.rs      ← 实验性 S3 上传
```

CLI（`polariseq-cli/src`）：

- `main.rs`（2238 行）：clap 定义 + 调度 + 日志基础设施 + 终端 UI 协调（GLOBAL_MP / MpWriter / BARS_ACTIVE）
- `ncbi_download.rs`（832 行）：NCBI 基因组子命令 —— **GSA resolver 的最佳参照骨架**
- `ui_manager.rs`（452 行）：底部全局状态条，实现 `DownloadObserver`
- `http_server.rs`（71 行）：AES-256-GCM Progress API（axum）

GUI（`polariseq-gui/src-tauri/src`）：

- `app.rs`（999 行）：全部 Tauri command + `DownloadEvent`/`UploadEvent` 事件枚举
- `lib.rs:3-6`：`pub use polariseq_core::*` 直接平铺 re-export
- `main.rs:14-28`：command 注册表（invoke_handler）
- 前端：`crates/polariseq-gui/src/App.tsx`（1584 行，单文件 React）

---

## 2. 事实清单逐条结论（A/B/C/D）

### A. 下载层（复用评估）

**A1. HTTP Range 多线程下载器位置与接口**

`ResumableDownloader`，crates/polariseq-core/src/aws_s3.rs:286-298。

```rust
// aws_s3.rs:301-342
pub async fn new(
    run_id: String,
    metadata: SraMetadata,        // {s3_uri, http_url, md5: Option<String>, size}
    save_dir: PathBuf,
    chunk_size_mb: u64,
    max_workers: usize,
    mp: Option<Arc<MultiProgress>>,
    progress_store: Option<ProgressStore>,
) -> Result<Self>
// builder:
.with_progress_bytes(Arc<AtomicU64>)   // aws_s3.rs:344-347（GUI/状态条共享字节计数）
.with_pause_token(PauseToken)          // aws_s3.rs:349-352（GUI 暂停）
// 入口:
pub async fn start() -> Result<bool>    // aws_s3.rs:387，true=文件完好
```

**不绑死 NCBI S3**：下载只使用 `metadata.http_url`（aws_s3.rs:543、766）；`SraUtils::get_metadata`/`parse_sra_xml`/`ncbi_rate_limit_wait` 全部独立于下载器（aws_s3.rs:94-284）。测试证明对任意 S3 https URL 通用（aws_s3.rs:882-907，kraken2 tar.gz）。**public-data 已把通用 https 复用打通**：`PublicDataDownloader::download_object` 构造 `SraMetadata` 后直接调它（downloader.rs:505-525）。GSA 可完全照抄该调用。

**A2. .meta.json 断点续传与 chunk 重试耦合度**

全部内聚在 `ResumableDownloader` 私有方法内：

- `load_progress/save_progress`（aws_s3.rs:355-372）：`.meta.json` = `filepath.with_extension("meta.json")`（aws_s3.rs:318），内容 `{"downloaded_chunks":[…]}`
- chunk 重试 `MAX_CHUNK_RETRIES = 3`（aws_s3.rs:596-627），失败 requeue，3 次后 fatal
- chunk 内偏移续传（aws_s3.rs:743-875）：`current_offset` 从中断处续，最多 50 总尝试（aws_s3.rs:738）

**新增普通 https 文件源：下载/续传/重试零改动。** 唯二注意点：

1. 文件名取自 `metadata.s3_uri` 最后一段（aws_s3.rs:310-316），GSA 需构造形如 `gsa://bucket/CRR123_1.fastq.gz` 的伪 URI
2. `with_extension("meta.json")` 会把 `xxx.fastq.gz` 变成 `xxx.fastq.meta.json`（无害但需知晓）

**A3. FTP 模式**

crates/polariseq-core/src/ftp.rs:19-218，**独立实现，本质是外部 `wget -c` 子进程**（ftp.rs:74-78），不经过 HTTP 下载器；进度靠 500ms 轮询文件大小（ftp.rs:122-129），完成后用自带 `verify_md5`（ftp.rs:220-236）校验。无断点元数据、依赖系统 wget。**建议 GSA 默认走 https + ResumableDownloader**。

**A4. 进度上报统一抽象（三通道）**

| 通道 | 接入点 | 证据 |
|---|---|---|
| CLI 进度条 | 下载器自建 per-file bar + `with_progress_bytes` 共享计数器 | aws_s3.rs:451-462；ncbi-download 用 `UiManager::transfer_bar`（ncbi_download.rs:545、ui_manager.rs:119-128） |
| GUI 事件 | GUI 自 spawn 500ms monitor 轮询 `progress_bytes` 后 emit `DownloadEvent::Progress` | app.rs:452-482 |
| HTTP Progress API | `ProgressStore` 由下载器内部 monitor 每 100ms 写入；axum 端 AES-256-GCM 加密 | aws_s3.rs:516-530；http_server.rs:24-71；密钥 build.rs:13-38（`POLARISEQ_PROGRESS_KEY` 可覆盖） |

统一观察者：`DownloadObserver` trait（observer.rs:25-43），CLI 实现于 ui_manager.rs:259-294。GSA 只要给 `ResumableDownloader` 传 `mp`/`progress_bytes`/`progress_store` 即全兼容。

### B. 校验层（复用评估）

**B5. md5 generate/verify 核心逻辑位置与暴露形式**

在 **core 库**，crates/polariseq-core/src/md5.rs，**以库函数暴露**（`generate_md5_manifest`/`verify_md5_manifest`/`verify_md5_entries` 均 pub）。CLI `md5` 子命令只是薄壳（main.rs:963-1014）。

ncbi-download 内建校验调用链：

```
download_assembly (ncbi_download.rs:226-281)
  → download_md5_manifest        ncbi_download.rs:283-302（下载 md5checksums.txt）
  → parse_md5_manifest            md5.rs:92-121
  → select_downloaded_md5_entries ncbi_download.rs:304-328（只挑已下载文件的条目）
  → verify_md5_entries            ncbi_download.rs:266-267 → md5.rs:342-418（4 线程）
  → failed > 0 ⇒ Err ⇒ 非零退出   ncbi_download.rs:268-274 → main.rs:813-832 ExitCode::FAILURE
```

**GSA 因官方 MD5 来自 API 字段而非清单文件，可跳过前两步，直接把 `(md5, filename)` 向量喂给 `verify_md5_entries`。**

**B6. md5.txt（md5sum 兼容清单）生成入口**

两个入口：

- `generate_md5sum_file_at(md5_path, files)`（lib.rs:501-523）：给定文件列表写任意路径，`"<md5>  <filename>\n"` 格式。AWS 下载完成后 CLI 调 `generate_md5sum_file`（lib.rs:497-499，main.rs:2014-2016）；public-data 调 `_at` 变体写 `<name>.md5`（downloader.rs:555-576）
- `generate_md5_manifest(target, output, threads, progress)`（md5.rs:184-262）：目录递归收集再生成

GSA 建议用 `generate_md5sum_file_at`（文件清单已知，不必递归）。

### C. 元数据解析层（参照骨架）

**C7. ncbi-download 完整链路（GSA 的模板）**

1. `run`（ncbi_download.rs:117-186）：校验编号（:121-123）→ 并发 `prepare_assembly_downloads`（:133-153）→ `ui.set_total`（:155-162）→ 并发 `download_assembly`（:164-171）→ 聚合失败返回 Err（:172-182）
2. `prepare_assembly_downloads`（ncbi_download.rs:201-224）：`assembly_ftp_directory`（esearch→esummary→ftppath，:352-416）→ `fetch_directory_listing`（:431-442）→ `ftp_file_urls` 按后缀选文件（:444-488）→ `inspect_downloads` 用 Range `bytes=0-0`/HEAD 预取大小（:490-525）→ 拼 `md5checksums.txt` URL（:215-217）
3. `download_assembly`（:226-281）：下载清单 → 逐文件 `download_with_progress`（:535-600，3 次整文件重试 + UiManager 计数）→ `verify_md5_entries`（:253-267）→ 失败即 Err（:268-274）

GSA 对应物：GSA API 返回清单（替代 1-2 的两步网络查询），下载/校验段（3）结构可近乎照抄。

**C8. ENA/EBI API 封装位置与 GSA resolver 放置**

`fetch_ena_data`（lib.rs:199-224）——reqwest GET + csv TSV 反序列化，**无重试**（与 NCBI get_metadata 形成对比）。

**GSA resolver 放 `crates/polariseq-core/src/gsa/`（新目录模块）最合适**：

- 结构参照 `ncbi_download.rs`（client 注入 + `fetch_json` 帮助函数 + 顶层 `run()`）
- 重试策略参照 `SraUtils::get_metadata`（aws_s3.rs:129-200：全局限速 aws_s3.rs:94-112 + Retry-After/指数退避/jitter aws_s3.rs:160-197）——NGDC 同样存在限流风险

**C9. CLI 子命令注册方式（clap derive）**

新增 `gsa-download` 需改 4 处，全在 crates/polariseq-cli/src/main.rs：

1. `Commands` enum 加变体（main.rs:115-131）
2. `mod gsa_download;`（main.rs:30-32 旁）
3. dispatch match 加分支（main.rs:800-811）
4. `output_dir`/`download_output` match 加分支（main.rs:716-742，决定日志目录与输出目录预建）+ 日志 tag（main.rs:752-765）

另参照 `run_ncbi_download` 的 UiManager 包装模式（main.rs:898-911）。

**C10. 错误处理与日志约定**

- 全部 `anyhow::Result` + `context`（无 thiserror）
- `tracing` 的 `info/warn/debug`；细粒度日志用 `debug!(target: "download_detail", …)`（aws_s3.rs:473；main.rs:1413-1416 注释确认该 target 被 stdout filter 保留）
- 终端渲染经 `MpWriter` 走 `MultiProgress::println`（main.rs:545-576）；日志文件名 `polariseq_<tag>_<ts>.log`（main.rs:1376-1383）

新模块必须遵守：函数返回 `anyhow::Result`，用户可见信息 `info!`，详单 `debug!(target: "download_detail")`。

### D. 配置与 GUI

**D11. public-data YAML 解析与 GSA 扩展点**

- `PublicDatabase`（config.rs:30-44：`s3_url/description/database_type/include/exclude/validate`）
- 由 `Config.public_data: HashMap<String, PublicDatabase>`（lib.rs:29）承载
- `should_download_key` 做 include/exclude wildcard（s3.rs:48-54）

**GSA 做成 YAML 可配置源的扩展点不在 public_data**——它整个 downloader 假定 S3：`parse_s3_url` 强制 `s3://` 前缀（s3.rs:16）、匿名 S3 client（downloader.rs:96-99）、`md5_from_etag`（downloader.rs:609-613）。GSA 走独立子命令 + core 内独立 resolver 模块更干净；YAML 仅在需要配置 NGDC 镜像/endpoint 时复用 `load_config`。

**D12. GUI Download 页前后端通信与最小改动集**

- Tauri command（`#[tauri::command]`）+ 事件流 `app_handle.emit("download-event", DownloadEvent{…})`
- 前端 `invoke('start_download_command', …)`（App.tsx:395-446）与 `listen('download-event')`（App.tsx:159）
- 注册表：crates/polariseq-gui/src-tauri/src/main.rs:14-28

**GSA 若要进 GUI，最小集**：

1. `app.rs` 新增 `start_gsa_download_command`（复刻 app.rs:260-322 的状态锁 + tokio::spawn 模式）
2. 下载体复刻 `download_aws`（app.rs:398-670）但**去掉 fasterq-dump 段（app.rs:517-565）与压缩段（app.rs:567-611）**——GSA 直接得到 fastq.gz
3. `main.rs`（GUI）invoke_handler 加一行
4. `App.tsx` 下载页（DownloadTab，App.tsx:774 起）加源切换
5. ⚠️ percent 映射：app.rs:460 的 `percent = bytes/total*50.0` 是硬编码给"下载占一半、转换+压缩占另一半"的，GSA 下载-only 流程应为 `*100.0`

---

## 3. 复用矩阵（GSA 需求 × 现有实现）

| GSA 需求 | 现有 | 说明 |
|---|---|---|---|
| **resolver**（CRA/CRR/URL→文件清单） | 无 GSA 封装；参照 `fetch_ena_data` lib.rs:199-224 与 `assembly_ftp_directory` ncbi_download.rs:352-416 | 【需新写-NGDC API 无任何封装】 | 编号/URL 解析参照 `validate_accession` ncbi_download.rs:330-350 的风格 |
| **下载**（https fastq.gz 多线程） | `ResumableDownloader::start` aws_s3.rs:387-656；调用示范 downloader.rs:488-553 | 【可直接复用-aws_s3.rs:286】 | 下载器只吃 `SraMetadata{http_url,md5,size}`，与 NCBI 无耦合；`verify_integrity` 内建官方 MD5 校验（aws_s3.rs:657-727） |
| **续传**（.meta.json + chunk 重试） | aws_s3.rs:355-372、596-627、743-875 | 【可直接复用-aws_s3.rs:355】 | 全部私有内聚；仅需喂对 `s3_uri`（文件名来源 aws_s3.rs:310-316），构造 `gsa://…/<filename>` 即可，无需改代码 |
| **校验**（官方 MD5→失败非零退出） | `verify_md5_entries` md5.rs:342-418；ncbi 调用链 ncbi_download.rs:253-274 | 【可直接复用-md5.rs:342】 | GSA API 的 (md5,filename) 直接作 entries；"校验失败→Err→ExitCode::FAILURE"流程照抄 |
| **清单输出**（md5.txt） | `generate_md5sum_file_at` lib.rs:501-523 | 【可直接复用-lib.rs:502】 | 已是 pub；public-data 已示范带路径调用（downloader.rs:573-574） |
| **CLI**（gsa-download 子命令） | clap derive；`Commands` main.rs:115-131 + ncbi_download.rs 全文件结构 | 【需小改-注册 4 处 + 新写子命令体】 | 新文件 `gsa_download.rs` 复刻 ncbi_download.rs 的 run/prepare/download/verify 四段 |
| **进度**（CLI 条 + GUI 事件 + HTTP API） | aws_s3.rs:451-462、app.rs:452-482、aws_s3.rs:516-530 + http_server.rs:24-35 | 【可直接复用-observer.rs:25】 | 给 downloader 传 `mp`/`progress_bytes`/`progress_store` 即通；ncbi-download 另示范 `UiManager::transfer_bar`+observer 用法（ncbi_download.rs:544-599） |
| **GUI**（Download 页新数据源） | Tauri command + `download-event` 事件（app.rs:88-114、App.tsx:159/395） | 【需小改-percent 映射 + 砍转换段】 | app.rs:460 的 `*50.0` 硬编码需改 `*100.0`（下载-only 流程） |
| **编号/URL 输入校验** | `validate_accession` ncbi_download.rs:330-350 | 【可直接复用-模式】 | CRA/CRR/Run-URL 各写一个校验函数，同款错误消息风格 |
| **并发控制**（文件级+chunk 级） | `stream::buffer_unordered` ncbi_download.rs:133-171；Semaphore downloader.rs:362 | 【可直接复用-ncbi_download.rs:149】 | 照 ncbi_download 的两层并发即可 |
| **失败聚合**（部分失败非零退出） | ncbi_download.rs:172-182、main.rs:2018-2025 | 【可直接复用-ncbi_download.rs:176】 | 模式一致 |

**矩阵结论：所有 GSA 需求均落入三档之一，无一项需要改动下载器本体。**

---

## 4. 接入点清单（新增 gsa-download 子命令，按改动量排序）

### 新建（改动量从大到小）

1. **`crates/polariseq-core/src/gsa/mod.rs`**（+ 可再拆 `resolver.rs`）
   - `parse_gsa_input(s) -> GsaAccession`（CRA##### / CRR###### / `https://ngdc.cncb.ac.cn/gsa/browse/…` → 规范化，风格参照 ncbi_download.rs:330-350）
   - `fetch_gsa_files(client, accession) -> Result<Vec<GsaFile>>`，`GsaFile{filename, size, https_url, ftp_url, md5}`；HTTP 侧参照 `fetch_json` ncbi_download.rs:418-429，重试参照 aws_s3.rs:129-200
   - 在 `lib.rs:3-11` 的 mod 列表加 `pub mod gsa;`

2. **`crates/polariseq-cli/src/gsa_download.rs`**
   - `GsaDownloadArgs`（clap Args，参照 ncbi_download.rs:59-105：accessions 多值、`-o`、`-p`；过滤可参照 s3.rs:48-54 的 wildcard）
   - `run(args, ui)`：prepare（调 resolver）→ 并发下载（`ResumableDownloader::new(…)` + `with_progress_bytes`，照抄 downloader.rs:505-525）→ `verify_md5_entries`（照抄 ncbi_download.rs:253-274）→ `generate_md5sum_file_at`（lib.rs:502）

### 修改（既有文件，改动量从小到大）

3. **`crates/polariseq-cli/src/main.rs`**
   - `mod gsa_download;`（:30-32 处）
   - `Commands::GsaDownload(GsaDownloadArgs)`（:115-131）
   - dispatch 分支 `run_gsa_download(args)`（:800-811），包装函数照 `run_ncbi_download`（:898-911）
   - `output_dir`/`download_output` match 分支（:716-742）+ 日志 tag（:752-765，建议传首编号）

4. **（可选，GUI 支持）**
   - `crates/polariseq-gui/src-tauri/src/app.rs`：`start_gsa_download_command` + `run_gsa_download_async`（复刻 app.rs:260-322/324-392，砍掉 fasterq-dump 段 app.rs:517-565 与压缩段 app.rs:567-611，percent 用 bytes/total*100）
   - `crates/polariseq-gui/src-tauri/src/main.rs:14-28` 注册
   - `crates/polariseq-gui/src/App.tsx`（DownloadTab :774 起）加源选择

5. **（可选，网络健康检查）** `main.rs:675-706` 的 `check_network_health`/`needs_network_health_check`（:708-710）加 NGDC 域名

### 完全不用动的文件

`aws_s3.rs`（下载器）、`md5.rs`、`ftp.rs`、`observer.rs`、`progress.rs`、`progress_store.rs`、`public_data/*`、`http_server.rs`、`ui_manager.rs`、`build.rs`。

---

## 5. 风险清单（按严重度分级）

### 阻断级（开工前必须确认）

1. **GSA 服务器必须支持 HTTP Range（206 + 标准 Content-Range）**。下载器强校验 `status == 206` 且 `Content-Range` 以 `bytes {start}-{end}/` 开头，否则按错误重试 10 次后放弃（aws_s3.rs:797-814）。NGDC 的 https 下载端点若不支持 Range，整个复用方案不成立——**先手工 `curl -r 0-0` 验证**。
2. **`metadata.size` 必须准确非零**：下载器用 `set_len(size)` 预分配（aws_s3.rs:434）并以 `size.div_ceil(chunk_size)` 计算 chunk（aws_s3.rs:438）；size=0 会导致 0 chunk 直接进入校验分支（aws_s3.rs:475-486）。GSA API 的 size 字段缺失时必须先 HEAD/Range 探测（照抄 `remote_file_size` ncbi_download.rs:503-525）。
3. **GSA API 形态未知**：CRA→Run 列表→文件清单的具体端点/分页/鉴权需先行调研；代码侧容器已备好，API 侧是纯新写。

### 需注意

4. **文件名派生自 `s3_uri`**（aws_s3.rs:310-316）：GSA 调用方构造 `s3_uri` 时必须以 `/文件名` 结尾，否则落盘名错误（用 run_id 兜底）。测试 aws_s3.rs:882-907 展示了期望行为。
5. **`.meta.json` 扩展名替换**：`with_extension("meta.json")` 把 `CRR1_1.fastq.gz` 的 `gz` 替换为 `meta.json` → `CRR1_1.fastq.meta.json`（aws_s3.rs:318）。无害，但 GSA 侧的清理/去重逻辑不要假设 meta 文件名格式。
6. **下载器 client 强制 `http1_only`**（aws_s3.rs:324）：对 NGDC CDN 兼容性大概率无碍，但如遇服务器仅 HTTP/2 需去掉该行（改动一行）。
7. **GUI percent 硬编码**：`* 50.0`（app.rs:460）假设下载只占一半进度（后接转换+压缩）。GSA 无转换环节，照抄会永远停在 50% 附近，必须另写下载-only 分支用 `*100.0`。
8. **HTTP Progress API 只对 `download -d aws` 生效**：ProgressStore 仅由 `download_with_aws` 预填充（main.rs:1650-1666）且 `--progress-port` 只挂在 DownloadArgs（main.rs:231-237）；gsa-download 想暴露进度 API 需自己预填 store 并复用 `start_progress_server`（http_server.rs:24-35）。
9. **`fetch_ena_data` 无重试**（lib.rs:205-213，裸 send + status 检查）：GSA resolver 不要照抄它，应照 `SraUtils::get_metadata` 的重试+jitter 模式（aws_s3.rs:141-199），NGDC 有限流可能。
10. **FTP 模式依赖系统 `wget`**（ftp.rs:74）：若 GSA 提供 FTP 兜底下载并想复用该路径，需在文档/依赖检查中声明 wget。

### 风格建议

11. GSA 子命令应照 ncbi_download.rs 组织：`prepare_*`（纯解析）与 `download_*`（带 UI）两阶段 + 顶层 `run()` 聚合失败（ncbi_download.rs:133-186）；不要把逻辑塞进 CLI main.rs（该文件已 2238 行）。
12. 编号校验错误消息保持 `expected …` 句式（ncbi_download.rs:333/344）；单文件下载重试常量放文件顶部 const（ncbi_download.rs:23-26 的做法）。
13. resolver 返回的清单建议直接映射为 `SraMetadata`（aws_s3.rs:28-33）+ `(md5, filename)` 两个视图，下载与校验各自消费，避免中间结构转换。

---

## 6. 结论：README 宣称 vs 代码实际

| README 宣称 | 代码证据 | 判定 |
|---|---|---|
| HTTP Range 多线程 + .meta.json 断点续传 | aws_s3.rs:387-656、355-372 | ✅ 一致 |
| FTP 下载模式 | ftp.rs:19-218（`wget -c` 子进程） | ✅ 存在，但**README 称"唯一外部依赖是 sra-tools"（README.md:105）与代码不符**——FTP 模式隐式依赖系统 wget（ftp.rs:74-78），以代码为准 |
| md5 generate/verify 子命令 | md5.rs:184-262/292-418 + main.rs:949-1014 | ✅ 一致，且为库函数可内建复用 |
| ncbi-download（accession→元数据→FTP 文件→官方 md5checksums.txt 校验） | ncbi_download.rs:117-281 | ✅ 一致 |
| public-data（YAML + include/exclude） | config.rs:30-44、s3.rs:48-54、downloader.rs:139-262 | ✅ 一致 |
| HTTP Progress API（AES-256-GCM） | http_server.rs:14-71、build.rs:13-38 | ✅ 一致 |
| Tauri GUI | app.rs 全文、main.rs:14-28 | ✅ 一致 |
| "下载完成"诚实失败处理（v1.4.2 宣称） | ncbi_download.rs:172-182、main.rs:2018-2025、app.rs:624-668 | ✅ 一致 |
| 仓库整洁度 | 存在遗留目录 `crates/ebidownload-gui/`（旧名，含 node_modules，不在 workspace 成员中，见 Cargo.toml:2-7） | ⚠️ 改名历史死目录，与本任务无关但建议清理 |

**总评**：代码库分层清晰（core 无 CLI/GUI 依赖，UI 通过 `DownloadObserver` trait 反转，observer.rs:1-6 的模块注释明确了这一约定），下载器已通过 public-data 验证了对非 SRA 场景的通用性。**GSA 接入的全部新写工作集中在 resolver（NGDC API 封装）与 CLI/GUI 注册层，下载、续传、校验、清单、进度五块可零改动或纯调用复用。**

**开工前唯一硬前置：用 curl 验证 NGDC https 端点支持 Range 请求。**
