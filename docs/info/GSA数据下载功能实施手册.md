# GSA 数据下载与校验功能 · 实施手册（任务书）

> 版本：2026-10-08 v1.0（依据 Polariseq v1.4.2 架构梳理报告定稿）
> 用法：本手册自包含。阶段 0 由用户手工执行；阶段 R1 整段提示词粘给编码 Agent；
> 每阶段验收权在用户手里，不接受编码 Agent 自报通过。
> 活文档《GSA数据下载与校验功能实现计划.md》保留全部历史决策，本手册仅含施工所需。

## 1. 任务目标

在 Polariseq（Rust workspace：crates/polariseq-core / polariseq-cli / polariseq-gui）中
新增 GSA（NGDC 国家生物信息中心）数据源下载支持：

**输入** GSA 编号（CRA 项目号 / CRR Run 号 / Run 页 URL）→ **解析**出文件清单
（文件名/大小/HTTPS/FTP 地址/官方 MD5）→ **下载** fastq.gz（无 .sra 转换环节）→
**MD5 校验**（不一致即非零退出）→ 输出 md5sum 兼容清单。

参照数据：CRR1235475（项目 CRA017017，别名 L105BSseq），2 个 fastq.gz 各约 2.4 GB，
下载页：https://ngdc.cncb.ac.cn/gsa/browse/CRA017017/CRR1235475

## 2. 已定稿的技术事实（架构梳理报告结论，编码 Agent 必读）

**代码结构**：core 无 CLI/GUI 依赖；UI 通过 `DownloadObserver` trait（observer.rs:25-43）
反转；全部 anyhow::Result + tracing（info! 用户可见 / debug!(target: "download_detail") 细节）。

**复用矩阵（全部经代码核实）**：

| 需求 | 复用对象 | 证据位置 | 结论 |
|---|---|---|---|
| https 多线程下载 + 断点续传 | `ResumableDownloader`（只消费 `SraMetadata{http_url,md5,size}`，与 NCBI 零耦合） | aws_s3.rs:286-728；public-data 调用示范 downloader.rs:505-525 | 直接复用，零改动 |
| 官方 MD5 校验 | `verify_md5_entries`（库函数，4 线程） | md5.rs:342-418；ncbi 调用链 ncbi_download.rs:253-274 | 直接复用 |
| md5sum 清单输出 | `generate_md5sum_file_at` | lib.rs:501-523 | 直接复用 |
| 三通道进度（CLI 条/GUI 事件/HTTP API） | 给下载器传 mp / progress_bytes / progress_store 三件套 | aws_s3.rs:451-462、516-530；app.rs:452-482 | 直接复用 |
| 并发与失败聚合 | `buffer_unordered` + Err 聚合 | ncbi_download.rs:133-182 | 照抄模式 |
| GSA resolver（NGDC API 封装） | 无 | —— | **唯一新写模块** |

**必须遵守的坑（报告风险清单，编码 Agent 逐条对照）**：

1. `s3_uri` 必须以 `/文件名` 结尾——落盘名从末段派生（aws_s3.rs:310-316）。GSA 调用方
   构造 `gsa://…/<文件名>` 形式。
2. `metadata.size` 必须准确非零：下载器 `set_len` 预分配 + 按 size 切 chunk（aws_s3.rs:434-438）。
   size 缺失/为 0 时先探测补齐（照抄 `remote_file_size`，ncbi_download.rs:503-525）。
3. 下载器强校验 Range：`status == 206` 且 Content-Range 以 `bytes {start}-{end}/` 开头，
   否则重试 10 次后放弃（aws_s3.rs:797-814）——**这是阶段 0 硬前置的由来**。
4. `.meta.json` 命名：`with_extension` 会把 `x.fastq.gz` 变成 `x.fastq.meta.json`（aws_s3.rs:318），
   无害但清理/去重逻辑不要假设 meta 文件名格式。
5. 下载器 client 强制 `http1_only`（aws_s3.rs:324）：NGDC 若仅 HTTP/2 需去掉该行（一行改动）。
6. 重试必须照 `SraUtils::get_metadata`（aws_s3.rs:129-200，限速+Retry-After/指数退避+jitter）；
   **禁止**照 `fetch_ena_data`（lib.rs:199-224，无重试裸请求）——NGDC 有限流风险。
7. FTP 模式 = 外部 `wget -c` 子进程（ftp.rs:74-78），无断点元数据。GSA 不走 FTP，
   FTP 地址仅作人工备用；若文档提及须声明系统 wget 依赖。
8. 校验失败 → Err → 非零退出，绝不报"下载成功"（ncbi_download.rs:253-274 诚实失败语义）。
9. 新代码风格：编号校验 `expected …` 句式（ncbi_download.rs:333/344）；重试常量放文件顶部
   const（ncbi_download.rs:23-26）；业务逻辑禁止塞进 main.rs（已 2238 行）。

**施工范围（写死）**：

- 新建 `crates/polariseq-core/src/gsa/mod.rs`（可拆 resolver.rs）+ lib.rs 注册 `pub mod gsa;`
- 新建 `crates/polariseq-cli/src/gsa_download.rs`
- 修改 `crates/polariseq-cli/src/main.rs` 四处：mod 声明（:30-32 旁）→ Commands enum 变体
  （:115-131）→ dispatch 分支（:800-811，包装照 run_ncbi_download :898-911）→
  output_dir/download_output match + 日志 tag（:716-742、:752-765）
- **完全不动**：aws_s3.rs、md5.rs、ftp.rs、observer.rs、progress*.rs、public_data/*、
  http_server.rs、ui_manager.rs、build.rs

## 3. 阶段 0：硬前置（用户手工执行，约 10 分钟，不过不开工）

| 步骤 | 操作 | 期望现象 | 不通过时处理 |
|---|---|---|---|
| 1 | `curl -r 0-0 -I "https://download.cncb.ac.cn/gsa4/CRA017017/CRR1235475/CRR1235475_f1.fq.gz"` | 返回 `206 Partial Content` + `Content-Range` 头 | **停下**：NGDC 不支持 Range 则下载器复用方案不成立，回头重议下载层 |
| 2 | 浏览器开 Run 页 → F12 → Network → XHR → 刷新页面 | 找到返回文件清单的 JSON 请求 | 找不到 → resolver 退化为解析 HTML 表格（选择器易碎，标注风险） |
| 3 | 查看该 JSON 响应字段 | 含文件名、大小、下载地址；确认有无 md5 字段 | 无 md5 → 校验按 UNKNOWN 方案（生成实际值供人工核对，不阻塞） |
| 4 | 接口换 CRA 项目号试一次 | 返回其下全部 Run（或确认仅支持 CRR 单查） | 仅支持单查 → CLI 收 CRA 时逐 Run 循环调用 |

阶段 0 结果回填：接口端点/参数/响应字段样例 ______；md5 字段有无 ______；
CRA 批量支持与否 ______。（回填后把事实补进本手册第 2 节，再进入 R1）

## 4. 阶段 R1：施工提示词（整段粘给编码 Agent）

```text
你是一名资深 Rust 工程师。在 Polariseq 中新增 GSA（NGDC）数据源下载支持。
代码库已完成架构梳理，以下 文件:行号 事实以梳理报告为准（v1.4.2），先读这些位置再动手。

## 铁律
- 禁止修改 aws_s3.rs / md5.rs / ftp.rs / observer.rs / progress*.rs / public_data/* /
  http_server.rs / ui_manager.rs / build.rs。
- 禁止把业务逻辑塞进 main.rs，逻辑全部放 core 或 gsa_download.rs。
- 实现以实际代码为准，README 仅为参考。

## 已完成的硬前置结论（由用户提供，直接采信）
- Range 验证：【通过/不通过】
- GSA 接口：端点【】；参数【】；响应字段【】；md5 字段【有/无】；
  CRA 批量【支持/不支持，不支持则逐 Run 循环】

## 新建 1：crates/polariseq-core/src/gsa/mod.rs（resolver）
- parse_gsa_input(s) -> GsaAccession：接受 CRA##### / CRR##### / Run 页 URL，
  风格照抄 validate_accession（ncbi_download.rs:330-350，含 expected … 句式）。
- fetch_gsa_files(client, accession) -> Vec<GsaFile{filename, size, https_url, ftp_url, md5}>
  - HTTP 帮助函数参照 ncbi_download.rs:418-429 的 fetch_json；
  - 重试照 SraUtils::get_metadata（aws_s3.rs:129-200，限速+Retry-After/指数退避+jitter），
    禁止照抄 fetch_ena_data（lib.rs:199-224 无重试）；
  - size 缺失或为 0 时必须探测补齐（照抄 remote_file_size，ncbi_download.rs:503-525）；
  - md5 拿不到时返回 null，不阻塞；
  - 输出双视图：to_sra_metadata()（s3_uri 用 gsa://…/<文件名>、以 /文件名 结尾，
    落盘名从末段派生 aws_s3.rs:310-316）+ (md5, filename) 向量（喂校验）。
- lib.rs mod 列表加 pub mod gsa;。

## 新建 2：crates/polariseq-cli/src/gsa_download.rs（子命令体）
- GsaDownloadArgs（clap Args，参照 ncbi_download.rs:59-105）：accessions 多值、-o、-p、--dry-run。
- run(args, ui) 四段结构照抄 ncbi_download.rs:117-281：
  1. prepare：调 resolver → 构造 SraMetadata 清单；--dry-run 打印（名称/大小/地址/md5 有无）后退出
  2. 并发下载：ResumableDownloader::new + with_progress_bytes，调用范式照抄 downloader.rs:505-525；
     文件级并发用 buffer_unordered（ncbi_download.rs:133-171）
  3. 校验：官方 (md5, filename) 直接喂 verify_md5_entries（md5.rs:342-418），
     failed>0 ⇒ Err（诚实失败语义，ncbi_download.rs:253-274）；md5=null 的文件调
     compute_md5 生成实际值、报告标 UNKNOWN，不阻塞
  4. 清单：generate_md5sum_file_at 写 md5.txt（lib.rs:501-523）
- 单文件重试常量放文件顶部 const（ncbi_download.rs:23-26 风格）。

## 修改：crates/polariseq-cli/src/main.rs 四处
1. mod gsa_download;（:30-32 旁）
2. Commands enum 加 GsaDownload 变体（:115-131）
3. dispatch 分支 run_gsa_download（:800-811），包装函数照 run_ncbi_download（:898-911）
4. output_dir/download_output match 分支（:716-742）+ 日志 tag（:752-765，用首编号）

## 约定
- anyhow::Result + context；info! 用户可见；debug!(target: "download_detail") 细节；
  终端渲染经 MpWriter / MultiProgress::println（main.rs:545-576）。
- 进度接线：给 downloader 传 mp / progress_bytes / progress_store 三件套
  （observer.rs:25-43），自动兼容 CLI 进度条、GUI 事件与 HTTP Progress API。

## 验收（逐条报告结果，不接受"应该没问题"）
- cargo build 通过、cargo clippy 无新增警告
- polariseq gsa-download CRR1235475 -o ./data --dry-run：打印 2 个 fastq 的名称/大小/地址/md5 有无
- 实跑 CRR1235475（约 4.7 GB）：下载完成 + md5.txt 生成 + 官方 md5 存在时校验 PASS
- 中断重跑：.meta.json 断点续传生效（文件继续增长而非从 0 开始）
- 篡改任一文件 1 字节再触发校验：FAIL + 非零退出
- 传不存在编号（CRR99999999）：明确报错、非零退出、不 panic
```

## 5. 阶段 R2：用户手工验收对照表

| # | 验收项 | 操作 | 期望现象 | 不通过时处理 |
|---|---|---|---|---|
| 1 | 编译 | `cargo build -p polariseq-cli --release` | 通过，无新增 clippy 警告 | 把报错原样贴回 |
| 2 | dry-run | `polariseq gsa-download CRR1235475 -o ./data --dry-run` | 打印 2 个 fastq（CRR1235475_f1/f2.fq.gz）的名称/大小/地址/md5 有无 | 对照第 2 节坑 1-3 排查 |
| 3 | 全量下载 | 实跑 CRR1235475 | 进度条正常推进；完成生成 md5.txt；官方 md5 存在时显示 PASS | 先看 `polariseq_gsa_<编号>.log` 再定性 |
| 4 | 断点续传 | 下载中断（Ctrl-C 或断网）后原命令重跑 | 文件继续增长，不从头开始 | 检查 .meta.json 是否生成（坑 4 命名规则） |
| 5 | 校验灵敏度 | 篡改任一文件 1 字节后触发校验 | 报 FAIL，退出码非零 | 检查是否走到 verify_md5_entries（坑 8） |
| 6 | 坏编号容错 | `polariseq gsa-download CRR99999999 -o ./data` | 明确报错（expected … 句式）、非零退出、不 panic | 报编码 Agent 修 resolver 错误处理 |
| 7 | 日志 | 查看输出目录日志 | info 可读、download_detail 细节在 debug 级 | 对照第 2 节约定 |

## 6. 后续可选阶段（本手册不含提示词，需要时再出）

- **R3 GUI 接入**：Download 页加数据源切换；⚠️ app.rs:460 的 `percent * 50.0` 硬编码
  （下载占一半、转换+压缩占另一半）必须改 `*100.0`——GSA 无转换环节；下载体复刻
  download_aws 但砍掉 fasterq-dump 段（app.rs:517-565）与压缩段（app.rs:567-611）。
- **R4 HTTP Progress API**：现仅对 `download -d aws` 生效（main.rs:1650-1666、:231-237）；
  gsa-download 想暴露需自预填 ProgressStore 并复用 start_progress_server（http_server.rs:24-35）。

## 7. 边界（本次明确不做）

- 不做 GSA 数据上传（NGDC 提交是另一套流程，与现有 NCBI SRA upload 无关）
- 不做 GSA-Human（GSAH，需授权）数据
- 不做 Qtrans 通道（官方标注"测试"）
- 不碰 .sra → fastq 转换与并行压缩链路（GSA 给的是 fastq.gz 成品）
- GUI 与 Progress API 等 CLI 稳定后再做

## 8. 风险与回退

| 风险 | 应对 |
|---|---|
| NGDC 不支持 Range | 阶段 0 第 1 步拦截；若发生，重议下载层（改 aws_s3.rs 或退回 wget 方案），成本另估 |
| GSA 接口变动 / HTML 改版 | resolver 集中一处，接口与 HTML 解析分离，改选择器不动下载/校验 |
| NGDC 限流 | 重试已按 SraUtils 模式（坑 6）；resolver 侧再加全局限速 |
| md5 字段拿不到 | UNKNOWN 降级：生成实际值 + 提示人工核对，不阻塞下载 |
| 磁盘占满 | 下载前检查剩余空间 ≥ 文件总大小 × 1.1，不足即拒绝 |
