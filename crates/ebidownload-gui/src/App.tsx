import { useState, useEffect } from 'react';
import { listen } from '@tauri-apps/api/event';
import { open } from '@tauri-apps/plugin-dialog';
import { invoke } from '@tauri-apps/api/core';
import './styles.css';

// Types
interface EnaRecord {
  run_accession: string;
  sample_title: string;
  library_layout: string | null;
}

interface Config {
  software: {
    prefetch: string;
    fasterq_dump: string;
  };
}

interface DownloadEvent {
  type: 'Log' | 'Progress' | 'Started' | 'Completed' | 'Error' | 'DryRun' | 'Metadata';
  data: any;
}

interface UploadEvent {
  type: 'Log' | 'Progress' | 'Started' | 'Completed' | 'Error' | 'DryRun';
  data: any;
}

function App() {
  const [activeTab, setActiveTab] = useState<'download' | 'upload' | 'settings'>('download');
  const [_config, setConfig] = useState<Config | null>(null);
  const [metadata, setMetadata] = useState<EnaRecord[]>([]);
  const [isDownloading, setIsDownloading] = useState(false);
  const [isUploading, setIsUploading] = useState(false);
  const [downloadProgress, setDownloadProgress] = useState<Record<string, { percent: number; status: string }>>({});
  const [uploadProgress, setUploadProgress] = useState<Record<string, { percent: number; status: string }>>({});
  const [logs, setLogs] = useState<{ level: string; message: string }[]>([]);

  // Download form state
  const [downloadForm, setDownloadForm] = useState({
    accession: '',
    tsv: '',
    output: '',
    method: 'Aws' as 'Ftp' | 'Prefetch' | 'Aws' | 'Auto',
    multithreads: 4,
    awsThreads: 8,
    chunkSize: 20,
    peOnly: false,
    cleanupSra: false,
    dryRun: false,
  });

  // Upload form state
  const [uploadForm, setUploadForm] = useState({
    bucket: '',
    prefix: '',
    files: [] as string[],
    region: 'us-east-1',
    concurrent: 4,
    applyPolicy: false,
    metadataTemplate: '',
    dryRun: false,
  });

  // Config form state
  const [configForm, setConfigForm] = useState({
    prefetchPath: '',
    fasterqDumpPath: '',
  });

  // Load config on startup
  useEffect(() => {
    loadConfig();
  }, []);

  // Setup event listeners
  useEffect(() => {
    const unlistenDownload = listen<DownloadEvent>('download-event', (event) => {
      handleDownloadEvent(event.payload);
    });

    const unlistenUpload = listen<UploadEvent>('upload-event', (event) => {
      handleUploadEvent(event.payload);
    });

    return () => {
      unlistenDownload.then(f => f());
      unlistenUpload.then(f => f());
    };
  }, []);

  const loadConfig = async () => {
    try {
      const result = await invoke<Config | null>('get_config_command');
      if (result) {
        setConfig(result);
        setConfigForm({
          prefetchPath: result.software.prefetch,
          fasterqDumpPath: result.software.fasterq_dump,
        });
      }
    } catch (e) {
      addLog('warn', `Failed to load config: ${e}`);
    }
  };

  const addLog = (level: string, message: string) => {
    setLogs(prev => [...prev.slice(-100), { level, message }]);
  };

  const handleDownloadEvent = (event: DownloadEvent) => {
    switch (event.type) {
      case 'Log':
        addLog(event.data.level, event.data.message);
        break;
      case 'Progress':
        setDownloadProgress(prev => ({
          ...prev,
          [event.data.run_id]: {
            percent: event.data.percent,
            status: event.data.status,
          },
        }));
        break;
      case 'Started':
        setIsDownloading(true);
        addLog('info', `Starting download of ${event.data.total} files`);
        break;
      case 'Completed':
        setIsDownloading(false);
        addLog('info', 'Download completed successfully');
        break;
      case 'Error':
        setIsDownloading(false);
        addLog('error', event.data.message);
        break;
      case 'Metadata':
        setMetadata(event.data.records);
        break;
      case 'DryRun':
        addLog('info', `Dry run: would download ${event.data.files.length} files`);
        break;
    }
  };

  const handleUploadEvent = (event: UploadEvent) => {
    switch (event.type) {
      case 'Log':
        addLog(event.data.level, event.data.message);
        break;
      case 'Progress':
        setUploadProgress(prev => ({
          ...prev,
          [event.data.filename]: {
            percent: event.data.percent,
            status: event.data.status,
          },
        }));
        break;
      case 'Started':
        setIsUploading(true);
        addLog('info', `Starting upload of ${event.data.total} files`);
        break;
      case 'Completed':
        setIsUploading(false);
        addLog('info', 'Upload completed successfully');
        break;
      case 'Error':
        setIsUploading(false);
        addLog('error', event.data.message);
        break;
      case 'DryRun':
        addLog('info', `Dry run: would upload ${event.data.files.length} files`);
        break;
    }
  };

  const handleFetchMetadata = async () => {
    try {
      const accession = downloadForm.accession || undefined;
      const tsv = downloadForm.tsv || undefined;
      const result = await invoke<EnaRecord[]>('fetch_metadata_command', { accession, tsv });
      setMetadata(result);
      addLog('info', `Fetched ${result.length} records`);
    } catch (e) {
      addLog('error', `Failed to fetch metadata: ${e}`);
    }
  };

  const handleStartDownload = async () => {
    if (!downloadForm.output) {
      addLog('error', 'Please select an output directory');
      return;
    }

    try {
      await invoke('start_download_command', {
        options: {
          accession: downloadForm.accession || null,
          tsv: downloadForm.tsv || null,
          output: downloadForm.output,
          downloadMethod: downloadForm.method,
          multithreads: downloadForm.multithreads,
          awsThreads: downloadForm.awsThreads,
          chunkSize: downloadForm.chunkSize,
          prefetchMaxSize: '100G',
          peOnly: downloadForm.peOnly,
          filterSample: [],
          filterRun: [],
          excludeSample: [],
          excludeRun: [],
          cleanupSra: downloadForm.cleanupSra,
          dryRun: downloadForm.dryRun,
        },
      });
    } catch (e) {
      addLog('error', `Failed to start download: ${e}`);
    }
  };

  const handleStartUpload = async () => {
    if (!uploadForm.bucket) {
      addLog('error', 'Please enter a bucket name');
      return;
    }
    if (uploadForm.files.length === 0) {
      addLog('error', 'Please select files to upload');
      return;
    }

    try {
      await invoke('start_upload_command', {
        options: {
          bucket: uploadForm.bucket,
          prefix: uploadForm.prefix || null,
          files: uploadForm.files,
          region: uploadForm.region,
          concurrent: uploadForm.concurrent,
          applyPolicy: uploadForm.applyPolicy,
          metadataTemplate: uploadForm.metadataTemplate || null,
          dryRun: uploadForm.dryRun,
        },
      });
    } catch (e) {
      addLog('error', `Failed to start upload: ${e}`);
    }
  };

  const handleSaveConfig = async () => {
    try {
      await invoke('save_config_command', {
        config: {
          prefetchPath: configForm.prefetchPath,
          fasterqDumpPath: configForm.fasterqDumpPath,
        },
      });
      addLog('info', 'Config saved successfully');
      await loadConfig();
    } catch (e) {
      addLog('error', `Failed to save config: ${e}`);
    }
  };

  const selectFile = async (multiple = false, filter?: string) => {
    const selected = await open({
      multiple,
      filters: filter ? [{ name: 'Files', extensions: [filter] }] : undefined,
    });
    return selected;
  };

  const selectDirectory = async () => {
    const selected = await open({
      directory: true,
    });
    return selected;
  };

  return (
    <div className="container">
      <div className="header">
        <h1>EBIDownload</h1>
        <p>Download and upload sequencing data</p>
      </div>

      <div className="tabs">
        <button
          className={`tab ${activeTab === 'download' ? 'active' : ''}`}
          onClick={() => setActiveTab('download')}
        >
          Download
        </button>
        <button
          className={`tab ${activeTab === 'upload' ? 'active' : ''}`}
          onClick={() => setActiveTab('upload')}
        >
          Upload
        </button>
        <button
          className={`tab ${activeTab === 'settings' ? 'active' : ''}`}
          onClick={() => setActiveTab('settings')}
        >
          Settings
        </button>
      </div>

      <div className="tab-content">
        {activeTab === 'download' && (
          <DownloadTab
            form={downloadForm}
            setForm={setDownloadForm}
            metadata={metadata}
            progress={downloadProgress}
            isDownloading={isDownloading}
            onFetchMetadata={handleFetchMetadata}
            onStartDownload={handleStartDownload}
            selectFile={selectFile}
            selectDirectory={selectDirectory}
          />
        )}

        {activeTab === 'upload' && (
          <UploadTab
            form={uploadForm}
            setForm={setUploadForm}
            progress={uploadProgress}
            isUploading={isUploading}
            onStartUpload={handleStartUpload}
            selectFile={selectFile}
          />
        )}

        {activeTab === 'settings' && (
          <SettingsTab
            form={configForm}
            setForm={setConfigForm}
            onSaveConfig={handleSaveConfig}
            selectFile={selectFile}
          />
        )}
      </div>

      <div className="log-container">
        {logs.map((log, i) => (
          <div key={i} className={`log-entry log-${log.level}`}>
            {log.message}
          </div>
        ))}
      </div>
    </div>
  );
}

// Download Tab Component
function DownloadTab({
  form,
  setForm,
  metadata,
  progress,
  isDownloading,
  onFetchMetadata,
  onStartDownload,
  selectFile,
  selectDirectory,
}: any) {
  const handleSelectTsv = async () => {
    const selected = await selectFile(false, 'tsv');
    if (selected && !Array.isArray(selected)) {
      setForm((prev: any) => ({ ...prev, tsv: selected }));
    }
  };

  const handleSelectOutput = async () => {
    const selected = await selectDirectory();
    if (selected && !Array.isArray(selected)) {
      setForm((prev: any) => ({ ...prev, output: selected }));
    }
  };

  return (
    <div className="download-tab">
      <div className="form-row">
        <div className="form-group">
          <label>Accession (e.g., PRJNA123456)</label>
          <input
            type="text"
            value={form.accession}
            onChange={(e) => setForm((prev: any) => ({ ...prev, accession: e.target.value }))}
            disabled={isDownloading}
            placeholder="PRJNA..."
          />
        </div>
        <div className="form-group">
          <label>Or TSV File</label>
          <div className="file-input">
            <input
              type="text"
              value={form.tsv}
              readOnly
              placeholder="Select a TSV file"
            />
            <button onClick={handleSelectTsv} disabled={isDownloading}>
              Browse
            </button>
          </div>
        </div>
      </div>

      <div className="form-group">
        <label>Output Directory</label>
        <div className="file-input">
          <input
            type="text"
            value={form.output}
            readOnly
            placeholder="Select output directory"
          />
          <button onClick={handleSelectOutput} disabled={isDownloading}>
            Browse
          </button>
        </div>
      </div>

      <div className="form-row">
        <div className="form-group">
          <label>Download Method</label>
          <select
            value={form.method}
            onChange={(e) => setForm((prev: any) => ({ ...prev, method: e.target.value as any }))}
            disabled={isDownloading}
          >
            <option value="Aws">AWS S3 (Fastest)</option>
            <option value="Ftp">FTP</option>
            <option value="Prefetch">Prefetch</option>
            <option value="Auto">Auto (AWS with fallback)</option>
          </select>
        </div>
        <div className="form-group">
          <label>Parallel Files</label>
          <input
            type="number"
            min={1}
            max={32}
            value={form.multithreads}
            onChange={(e) => setForm((prev: any) => ({ ...prev, multithreads: parseInt(e.target.value) }))}
            disabled={isDownloading}
          />
        </div>
        <div className="form-group">
          <label>Threads per File</label>
          <input
            type="number"
            min={1}
            max={32}
            value={form.awsThreads}
            onChange={(e) => setForm((prev: any) => ({ ...prev, awsThreads: parseInt(e.target.value) }))}
            disabled={isDownloading}
          />
        </div>
      </div>

      <div className="form-row">
        <div className="form-group">
          <label>
            <input
              type="checkbox"
              checked={form.peOnly}
              onChange={(e) => setForm((prev: any) => ({ ...prev, peOnly: e.target.checked }))}
              disabled={isDownloading}
            />
            PE Only (skip SE)
          </label>
        </div>
        <div className="form-group">
          <label>
            <input
              type="checkbox"
              checked={form.cleanupSra}
              onChange={(e) => setForm((prev: any) => ({ ...prev, cleanupSra: e.target.checked }))}
              disabled={isDownloading}
            />
            Cleanup SRA files
          </label>
        </div>
        <div className="form-group">
          <label>
            <input
              type="checkbox"
              checked={form.dryRun}
              onChange={(e) => setForm((prev: any) => ({ ...prev, dryRun: e.target.checked }))}
              disabled={isDownloading}
            />
            Dry Run
          </label>
        </div>
      </div>

      <div className="button-row">
        <button
          className="btn btn-secondary"
          onClick={onFetchMetadata}
          disabled={isDownloading || (!form.accession && !form.tsv)}
        >
          Fetch Metadata
        </button>
        <button
          className="btn btn-primary"
          onClick={onStartDownload}
          disabled={isDownloading || !form.output || (!form.accession && !form.tsv)}
        >
          {isDownloading ? 'Downloading...' : 'Start Download'}
        </button>
      </div>

      {metadata.length > 0 && (
        <div className="metadata-section">
          <h3>Records ({metadata.length})</h3>
          <div className="metadata-table">
            <table>
              <thead>
                <tr>
                  <th>Run Accession</th>
                  <th>Sample Title</th>
                  <th>Layout</th>
                  <th>Progress</th>
                </tr>
              </thead>
              <tbody>
                {metadata.map((record: EnaRecord) => (
                  <tr key={record.run_accession}>
                    <td>{record.run_accession}</td>
                    <td>{record.sample_title}</td>
                    <td>{record.library_layout || 'N/A'}</td>
                    <td>
                      {progress[record.run_accession] ? (
                        <div className="progress-cell">
                          <div className="progress-bar-small">
                            <div
                              className="progress-fill"
                              style={{ width: `${progress[record.run_accession].percent}%` }}
                            />
                          </div>
                          <span>{progress[record.run_accession].status}</span>
                        </div>
                      ) : (
                        <span className="pending">Pending</span>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}
    </div>
  );
}

// Upload Tab Component
function UploadTab({
  form,
  setForm,
  progress,
  isUploading,
  onStartUpload,
  selectFile,
}: any) {
  const handleSelectFiles = async () => {
    const selected = await selectFile(true);
    if (selected) {
      const files = Array.isArray(selected) ? selected : [selected];
      setForm((prev: any) => ({ ...prev, files }));
    }
  };

  return (
    <div className="upload-tab">
      <div className="form-group">
        <label>Bucket Name</label>
        <input
          type="text"
          value={form.bucket}
          onChange={(e) => setForm((prev: any) => ({ ...prev, bucket: e.target.value }))}
          disabled={isUploading}
          placeholder="my-bucket"
        />
      </div>

      <div className="form-group">
        <label>Key Prefix (optional)</label>
        <input
          type="text"
          value={form.prefix}
          onChange={(e) => setForm((prev: any) => ({ ...prev, prefix: e.target.value }))}
          disabled={isUploading}
          placeholder="path/to/files"
        />
      </div>

      <div className="form-group">
        <label>Files to Upload</label>
        <div className="file-input">
          <input
            type="text"
            value={form.files.length > 0 ? `${form.files.length} file(s)` : ''}
            readOnly
            placeholder="Select files"
          />
          <button onClick={handleSelectFiles} disabled={isUploading}>
            Browse
          </button>
        </div>
      </div>

      <div className="form-row">
        <div className="form-group">
          <label>Region</label>
          <input
            type="text"
            value={form.region}
            onChange={(e) => setForm((prev: any) => ({ ...prev, region: e.target.value }))}
            disabled={isUploading}
            placeholder="us-east-1"
          />
        </div>
        <div className="form-group">
          <label>Concurrent Uploads</label>
          <input
            type="number"
            min={1}
            max={32}
            value={form.concurrent}
            onChange={(e) => setForm((prev: any) => ({ ...prev, concurrent: parseInt(e.target.value) }))}
            disabled={isUploading}
          />
        </div>
      </div>

      <div className="form-row">
        <div className="form-group">
          <label>
            <input
              type="checkbox"
              checked={form.applyPolicy}
              onChange={(e) => setForm((prev: any) => ({ ...prev, applyPolicy: e.target.checked }))}
              disabled={isUploading}
            />
            Apply NCBI Policy
          </label>
        </div>
        <div className="form-group">
          <label>
            <input
              type="checkbox"
              checked={form.dryRun}
              onChange={(e) => setForm((prev: any) => ({ ...prev, dryRun: e.target.checked }))}
              disabled={isUploading}
            />
            Dry Run
          </label>
        </div>
      </div>

      <div className="button-row">
        <button
          className="btn btn-primary"
          onClick={onStartUpload}
          disabled={isUploading || !form.bucket || form.files.length === 0}
        >
          {isUploading ? 'Uploading...' : 'Start Upload'}
        </button>
      </div>

      {form.files.length > 0 && (
        <div className="file-list-section">
          <h3>Files ({form.files.length})</h3>
          <div className="file-list">
            {form.files.map((file: string, i: number) => {
              const filename = file.split(/[/\\]/).pop() || file;
              return (
                <div key={i} className="file-item">
                  <span>{filename}</span>
                  {progress[filename] ? (
                    <div className="progress-cell">
                      <div className="progress-bar-small">
                        <div
                          className="progress-fill"
                          style={{ width: `${progress[filename].percent}%` }}
                        />
                      </div>
                      <span>{progress[filename].status}</span>
                    </div>
                  ) : (
                    <span className="pending">Pending</span>
                  )}
                </div>
              );
            })}
          </div>
        </div>
      )}
    </div>
  );
}

// Settings Tab Component
function SettingsTab({
  form,
  setForm,
  onSaveConfig,
  selectFile,
}: any) {
  const createFileSelector = (field: string) => async () => {
    const selected = await selectFile(false);
    if (selected && !Array.isArray(selected)) {
      setForm((prev: any) => ({ ...prev, [field]: selected }));
    }
  };

  return (
    <div className="settings-tab">
      <div className="form-group">
        <label>Prefetch Path</label>
        <div className="file-input">
          <input
            type="text"
            value={form.prefetchPath}
            onChange={(e) => setForm((prev: any) => ({ ...prev, prefetchPath: e.target.value }))}
            placeholder="/path/to/prefetch"
          />
          <button onClick={createFileSelector('prefetchPath')}>Browse</button>
        </div>
      </div>

      <div className="form-group">
        <label>fasterq-dump Path</label>
        <div className="file-input">
          <input
            type="text"
            value={form.fasterqDumpPath}
            onChange={(e) => setForm((prev: any) => ({ ...prev, fasterqDumpPath: e.target.value }))}
            placeholder="/path/to/fasterq-dump"
          />
          <button onClick={createFileSelector('fasterqDumpPath')}>Browse</button>
        </div>
      </div>

      <div className="button-row">
        <button className="btn btn-primary" onClick={onSaveConfig}>
          Save Config
        </button>
      </div>
    </div>
  );
}

export default App;
