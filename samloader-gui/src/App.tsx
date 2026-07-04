import {
  ArrowUDownLeft,
  ArrowsClockwise,
  Check,
  CheckCircle,
  Circle,
  Copy,
  DeviceMobile,
  DownloadSimple,
  FileArchive,
  FilePlus,
  FolderOpen,
  GearSix,
  Lightning,
  MagnifyingGlass,
  Moon,
  PlusCircle,
  SealCheck,
  Sun,
  Table,
  TerminalWindow,
  Usb,
  Warning,
  WarningOctagon,
} from "@phosphor-icons/react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open, save } from "@tauri-apps/plugin-dialog";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

type View = "download" | "flash" | "updates" | "device" | "verify" | "settings";
type Theme = "dark" | "light";
type FlashMode = "pkg" | "folder";
type CscMode = "home" | "wipe";
type RunState = "idle" | "active" | "done" | "error";

type FolderPackages = {
  bl?: string | null;
  ap?: string | null;
  cp?: string | null;
  csc?: string | null;
  userdata?: string | null;
};

type AppInfo = {
  version: string;
  defaultBackend: string;
  backends: string[];
};

type DownloadEvent = {
  status: string;
  filename?: string | null;
  path?: string | null;
  bytes: number;
  total: number;
  speedBps: number;
  message?: string | null;
};

type FlashEvent = {
  status: string;
  partition?: string | null;
  percent: number;
  partitionBytes?: number | null;
  partitionTotal?: number | null;
  overallBytes?: number | null;
  overallTotal?: number | null;
  message?: string | null;
};

type FlashPart = {
  pct: number;
  bytes: number;
  total: number;
};

type DeviceStatus = {
  connected: boolean;
  label: string;
};

type PitRow = {
  id: string;
  partition: string;
  flashFile: string;
  size: number;
};

type VerifyRow = {
  file: string;
  status: "pending" | "checking" | "ok" | "error";
  message: string;
};

const basename = (path: string) => path.split(/[\\/]/).filter(Boolean).pop() ?? path;

const quote = (value: string) => {
  if (!value) return "";
  return /[\s"]/g.test(value) ? `"${value.replaceAll('"', '\\"')}"` : value;
};

const fmtBytes = (bytes: number) => {
  if (!bytes) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(unit === 0 ? 0 : 2)} ${units[unit]}`;
};

const cscLabel = (mode: CscMode) => (mode === "wipe" ? "CSC" : "HOME_CSC");
const hasTauriRuntime = () =>
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

function packageList(packages: FolderPackages) {
  return [
    packages.bl,
    packages.ap,
    packages.cp,
    packages.csc,
    packages.userdata,
  ].filter(Boolean) as string[];
}

export function App() {
  const [appInfo, setAppInfo] = useState<AppInfo>({
    version: "2.0.0",
    defaultBackend: "vcom",
    backends: ["vcom", "nusb"],
  });
  const [theme, setTheme] = useState<Theme>("dark");
  const [view, setView] = useState<View>("download");
  const [backend, setBackend] = useState("vcom");
  const [verbose, setVerbose] = useState(false);
  const [copied, setCopied] = useState(false);

  const [deviceConnected, setDeviceConnected] = useState(false);
  const [deviceBusy, setDeviceBusy] = useState(false);
  const [pitRows, setPitRows] = useState<PitRow[]>([]);
  const [deviceMessage, setDeviceMessage] = useState("Odin / download mode");
  const deviceStatusInFlight = useRef(false);

  const [dModel, setDModel] = useState("SM-S931U1");
  const [dRegion, setDRegion] = useState("XAA");
  const [dVersion, setDVersion] = useState("");
  const [dOut, setDOut] = useState("");
  const [dThreads, setDThreads] = useState(8);
  const [downloadState, setDownloadState] = useState<RunState>("idle");
  const [downloadEvent, setDownloadEvent] = useState<DownloadEvent | null>(null);

  const [flashMode, setFlashMode] = useState<FlashMode>("folder");
  const [folder, setFolder] = useState("");
  const [cscMode, setCscMode] = useState<CscMode>("home");
  const [packages, setPackages] = useState<FolderPackages>({});
  const [flashOpts, setFlashOpts] = useState({
    noReboot: false,
    wait: false,
    skipMd5: false,
    skipSize: false,
    repartition: false,
  });
  const [flashModal, setFlashModal] = useState(false);
  const [flashState, setFlashState] = useState<RunState>("idle");
  const [flashMessage, setFlashMessage] = useState("");
  const [flashParts, setFlashParts] = useState<Record<string, FlashPart>>({});
  const [flashOverall, setFlashOverall] = useState({ bytes: 0, total: 0, pct: 0 });

  const [uModel, setUModel] = useState("SM-S931U1");
  const [uRegion, setURegion] = useState("XAA");
  const [updatesState, setUpdatesState] = useState<RunState>("idle");
  const [updates, setUpdates] = useState<{ latest: string; previous: string[]; beta: string[] } | null>(null);

  const [verifyRows, setVerifyRows] = useState<VerifyRow[]>([]);

  const refreshDeviceStatus = useCallback(async (wait = false, showBusy = false) => {
    if (deviceStatusInFlight.current) return;
    deviceStatusInFlight.current = true;
    if (showBusy) setDeviceBusy(true);

    try {
      const result = await invoke<DeviceStatus>("detect_download_device", { backend, wait });
      setDeviceConnected(result.connected);
      setDeviceMessage(result.label);
    } catch {
      setDeviceConnected(false);
      setDeviceMessage("Unable to check download-mode device");
    } finally {
      if (showBusy) setDeviceBusy(false);
      deviceStatusInFlight.current = false;
    }
  }, [backend]);

  useEffect(() => {
    if (!hasTauriRuntime()) return;

    invoke<AppInfo>("app_info")
      .then((info) => {
        setAppInfo(info);
        setBackend(info.defaultBackend);
      })
      .catch(() => undefined);

    const unlistenDownload = listen<DownloadEvent>("download-progress", (event) => {
      setDownloadEvent(event.payload);
      if (event.payload.status === "done") setDownloadState("done");
      else if (event.payload.status === "error") setDownloadState("error");
      else setDownloadState("active");
    });

    const unlistenFlash = listen<FlashEvent>("flash-progress", (event) => {
      const payload = event.payload;
      if (payload.overallTotal) {
        setFlashOverall({
          bytes: payload.overallBytes ?? 0,
          total: payload.overallTotal,
          pct: payload.percent,
        });
      } else if (payload.status === "done") {
        setFlashOverall((current) => ({ ...current, bytes: current.total, pct: 100 }));
      }
      setFlashMessage(payload.message ?? (payload.partition ? `Uploading ${payload.partition}` : payload.status));
      if (payload.partition) {
        const partitionBytes = payload.partitionBytes ?? 0;
        const partitionTotal = payload.partitionTotal ?? 0;
        const partitionPct = partitionTotal > 0
          ? (partitionBytes / partitionTotal) * 100
          : payload.status === "partition-done"
            ? 100
            : 0;
        setFlashParts((parts) => ({
          ...parts,
          [payload.partition as string]: {
            pct: Math.max(parts[payload.partition as string]?.pct ?? 0, partitionPct),
            bytes: partitionBytes,
            total: partitionTotal,
          },
        }));
      }
      if (payload.status === "done") setFlashState("done");
      else if (payload.status === "error") setFlashState("error");
      else setFlashState("active");
    });

    return () => {
      void unlistenDownload.then((fn) => fn());
      void unlistenFlash.then((fn) => fn());
    };
  }, []);

  useEffect(() => {
    if (!hasTauriRuntime() || flashState === "active") return;

    void refreshDeviceStatus(false, false);
    const interval = window.setInterval(() => {
      void refreshDeviceStatus(false, false);
    }, 2500);

    return () => window.clearInterval(interval);
  }, [flashState, refreshDeviceStatus]);

  const selectedPackages = packageList(packages);
  const command = useMemo(() => {
    const globals = `${backend ? ` --usb-backend ${backend}` : ""}${verbose ? " --verbose" : ""}`;
    if (view === "download") {
      return `samloader${globals} download -m ${dModel || "MODEL"} -r ${dRegion || "CSC"}${dVersion ? ` -v ${quote(dVersion)}` : ""} -j ${dThreads}${dOut ? ` -d ${quote(dOut)}` : ""}`;
    }
    if (view === "flash") {
      const opts = `${flashOpts.wait ? " --wait" : ""}${flashOpts.noReboot ? " --no-reboot" : ""}${flashOpts.skipMd5 ? " --skip-md5" : ""}${flashOpts.skipSize ? " --skip-size-check" : ""}${flashOpts.repartition ? " --repartition" : ""}`;
      if (flashMode === "folder") {
        return `samloader${globals} flash${opts} --folder ${quote(folder || "<folder>")}${cscMode === "wipe" ? " --csc-mode wipe" : ""}`;
      }
      return `samloader${globals} flash${opts}${packages.bl ? ` -b ${quote(packages.bl)}` : ""}${packages.ap ? ` -a ${quote(packages.ap)}` : ""}${packages.cp ? ` -c ${quote(packages.cp)}` : ""}${packages.csc ? ` -s ${quote(packages.csc)}` : ""}${packages.userdata ? ` -u ${quote(packages.userdata)}` : ""}`;
    }
    if (view === "updates") return `samloader${globals} check-update -m ${uModel || "MODEL"} -r ${uRegion || "CSC"} --all`;
    if (view === "device") return pitRows.length ? `samloader${globals} print-pit` : `samloader${globals} detect`;
    if (view === "verify") return `samloader verify-md5 ${verifyRows.map((row) => quote(row.file)).join(" ") || "<files>"}`;
    return `samloader${globals}`;
  }, [backend, cscMode, dModel, dOut, dRegion, dThreads, dVersion, flashMode, flashOpts, folder, packages, pitRows.length, uModel, uRegion, verbose, verifyRows, view]);

  const title = {
    download: ["Download firmware", "Fetch and decrypt official firmware over multiple connections."],
    flash: ["Flash firmware", "Write packages or an extracted folder to your device."],
    updates: ["Check for updates", "Look up latest, previous, and beta firmware versions."],
    device: ["Device & PIT", "Detect the device, inspect PIT, or reboot to download mode."],
    verify: ["Verify MD5", "Check downloaded .tar.md5 package integrity."],
    settings: ["Settings", "USB backend, logging, and appearance."],
  }[view];

  async function copyCommand() {
    await navigator.clipboard.writeText(command);
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1400);
  }

  async function pickDirectory(onPick: (path: string) => void) {
    const selected = await open({ directory: true, multiple: false });
    if (typeof selected === "string") onPick(selected);
  }

  async function pickPackage(key: keyof FolderPackages) {
    const selected = await open({
      multiple: false,
      filters: [{ name: "Odin package", extensions: ["tar", "md5"] }],
    });
    if (typeof selected === "string") {
      setPackages((current) => ({ ...current, [key]: selected }));
    }
  }

  async function scanFolder(path = folder, mode = cscMode) {
    if (!path.trim()) return;
    const result = await invoke<FolderPackages>("scan_firmware_folder", { folder: path, cscMode: mode });
    setPackages(result);
  }

  async function startDownload() {
    setDownloadState("active");
    setDownloadEvent({ status: "started", bytes: 0, total: 0, speedBps: 0, message: "Preparing download" });
    await invoke("start_download", {
      req: {
        model: dModel,
        region: dRegion,
        version: dVersion || null,
        outDir: dOut || null,
        outFile: null,
        threads: dThreads,
        verbose,
      },
    });
  }

  async function checkUpdates() {
    setUpdatesState("active");
    try {
      const result = await invoke<{ latest: string; previous: string[]; beta: string[] }>("check_updates", { model: uModel, region: uRegion });
      setUpdates(result);
      setUpdatesState("done");
    } catch {
      setUpdatesState("error");
    }
  }

  async function detectDevice(wait = false) {
    await refreshDeviceStatus(wait, true);
  }

  async function printPit() {
    setDeviceBusy(true);
    try {
      const rows = await invoke<PitRow[]>("read_device_pit", { backend, wait: true, verbose });
      setPitRows(rows);
      setDeviceConnected(true);
      setDeviceMessage("PIT loaded from device");
    } finally {
      setDeviceBusy(false);
    }
  }

  async function dumpPit() {
    const output = await save({ filters: [{ name: "PIT", extensions: ["pit"] }] });
    if (output) await invoke("dump_device_pit", { backend, wait: true, verbose, output });
  }

  async function rebootDownload() {
    setDeviceBusy(true);
    try {
      await invoke("reboot_to_download", { backend });
      setDeviceConnected(true);
      setDeviceMessage("Reboot command sent");
    } finally {
      setDeviceBusy(false);
    }
  }

  async function addVerifyFiles() {
    const selected = await open({
      multiple: true,
      filters: [{ name: "MD5 package", extensions: ["md5"] }],
    });
    const files = Array.isArray(selected) ? selected : typeof selected === "string" ? [selected] : [];
    setVerifyRows((rows) => [
      ...rows,
      ...files.map((file) => ({ file, status: "pending" as const, message: "Ready to verify" })),
    ]);
  }

  async function verifyAll() {
    setVerifyRows((rows) => rows.map((row) => ({ ...row, status: "checking", message: "Verifying..." })));
    const result = await invoke<Array<{ file: string; ok: boolean; message: string }>>("verify_md5_files", {
      files: verifyRows.map((row) => row.file),
    });
    setVerifyRows(result.map((row) => ({ file: row.file, status: row.ok ? "ok" : "error", message: row.message })));
  }

  async function confirmFlash() {
    setFlashModal(false);
    setFlashState("active");
    setFlashParts({});
    setFlashOverall({ bytes: 0, total: 0, pct: 0 });
    setFlashMessage(deviceConnected ? "Preparing flash" : "Waiting for device");
    await invoke("start_flash", {
      req: {
        backend,
        verbose,
        wait: flashOpts.wait || !deviceConnected,
        noReboot: flashOpts.noReboot,
        repartition: flashOpts.repartition,
        skipSizeCheck: flashOpts.skipSize,
        skipMd5: flashOpts.skipMd5,
        pit: null,
        folder: flashMode === "folder" ? folder : null,
        cscMode,
        packages,
      },
    });
  }

  const destructive = flashOpts.repartition || (flashMode === "folder" && cscMode === "wipe");
  const downloadPct = downloadEvent?.total ? Math.min(100, (downloadEvent.bytes / downloadEvent.total) * 100) : 0;
  const deviceStatusTitle = deviceConnected
    ? "Download mode device"
    : deviceBusy
      ? "Detecting..."
      : "No download-mode device";

  return (
    <div className="app" data-theme={theme}>
      <div className="titlebar">
        <div className="traffic"><span /><span /><span /></div>
        <div className="brand-mark"><Lightning weight="bold" /></div>
        <div className="brand">samloader</div>
        <div className="version">v{appInfo.version}</div>
        <div className="title-spacer" />
        <div className="tagline">Samsung firmware · download & flash</div>
      </div>

      <div className="shell">
        <aside className="sidebar">
          <NavGroup label="Firmware">
            <NavButton icon={<DownloadSimple />} label="Download" active={view === "download"} onClick={() => setView("download")} />
            <NavButton icon={<Lightning />} label="Flash" active={view === "flash"} onClick={() => setView("flash")} />
            <NavButton icon={<ArrowsClockwise />} label="Updates" active={view === "updates"} onClick={() => setView("updates")} />
          </NavGroup>
          <NavGroup label="Device">
            <NavButton icon={<DeviceMobile />} label="Device & PIT" active={view === "device"} onClick={() => setView("device")} />
            <NavButton icon={<SealCheck />} label="Verify MD5" active={view === "verify"} onClick={() => setView("verify")} />
          </NavGroup>
          <div className="sidebar-spacer" />
          <div className="device-card">
            <StatusDot connected={deviceConnected} busy={deviceBusy} />
            <div>
              <strong>{deviceStatusTitle}</strong>
              <span>{deviceMessage}</span>
            </div>
          </div>
          <div className="sidebar-actions">
            <button onClick={() => setView("settings")} className="ghost stretch"><GearSix /> Settings</button>
            <button onClick={() => setTheme(theme === "dark" ? "light" : "dark")} className="icon-button" title="Toggle theme">
              {theme === "dark" ? <Sun /> : <Moon />}
            </button>
          </div>
        </aside>

        <main className="main">
          <header className="header">
            <div>
              <h1>{title[0]}</h1>
              <p>{title[1]}</p>
            </div>
            <div className="device-pill"><StatusDot connected={deviceConnected} busy={deviceBusy} />{deviceStatusTitle}</div>
          </header>

          <section className={`content ${view === "flash" ? "flash-content" : ""}`}>
            {view === "download" && (
              <div className="stack narrow">
                <Card>
                  <div className="form-grid">
                    <Field label="Model"><input value={dModel} onChange={(e) => setDModel(e.target.value.toUpperCase())} /></Field>
                    <Field label="Region (CSC)"><input value={dRegion} onChange={(e) => setDRegion(e.target.value.toUpperCase())} /></Field>
                    <Field label="Version"><input value={dVersion} onChange={(e) => setDVersion(e.target.value)} placeholder="Latest available" /></Field>
                    <Field label="Output folder">
                      <div className="input-action">
                        <input value={dOut} onChange={(e) => setDOut(e.target.value)} placeholder="Default download folder" />
                        <button onClick={() => pickDirectory(setDOut)}><FolderOpen /></button>
                      </div>
                    </Field>
                  </div>
                  <div className="divider" />
                  <div className="inline-control">
                    <div className="grow">
                      <div className="row between"><span className="label">Parallel connections</span><code className="accent">{dThreads}</code></div>
                      <input className="range" type="range" min={1} max={16} value={dThreads} onChange={(e) => setDThreads(Number(e.target.value))} />
                    </div>
                    <button className="primary" onClick={startDownload}><DownloadSimple weight="bold" /> Start download</button>
                  </div>
                </Card>

                {downloadState !== "idle" && (
                  <Card rise>
                    <div className="row between">
                      <div className="row">
                        <StatusDot connected={downloadState === "done"} busy={downloadState === "active"} />
                        <div><code>{downloadEvent?.filename ?? "Preparing firmware"}</code><p>{downloadEvent?.message ?? "Downloading · decrypting on-the-fly"}</p></div>
                      </div>
                      <strong className="big-pct">{Math.round(downloadPct)}%</strong>
                    </div>
                    <Progress value={downloadPct} ok={downloadState === "done"} />
                    <div className="stats four">
                      <Stat label="Downloaded" value={fmtBytes(downloadEvent?.bytes ?? 0)} />
                      <Stat label="Total" value={fmtBytes(downloadEvent?.total ?? 0)} />
                      <Stat label="Speed" value={downloadEvent?.speedBps ? `${fmtBytes(downloadEvent.speedBps)}/s` : "--"} accent />
                      <Stat label="Output" value={downloadEvent?.path ? basename(downloadEvent.path) : "--"} />
                    </div>
                  </Card>
                )}
              </div>
            )}

            {view === "flash" && (
              <div className="stack wide flash-stack">
                <div className="flash-config">
                  <div className="segments">
                    <button className={flashMode === "pkg" ? "active" : ""} onClick={() => setFlashMode("pkg")}>Package files</button>
                    <button className={flashMode === "folder" ? "active" : ""} onClick={() => setFlashMode("folder")}>Firmware folder</button>
                  </div>

                  {flashMode === "folder" ? (
                    <Card className="flash-folder-card">
                      <Field label="Extracted firmware folder">
                        <div className="input-action">
                          <input value={folder} onChange={(e) => setFolder(e.target.value)} placeholder="D:\\SAMFW..." />
                          <button onClick={async () => {
                            const selected = await open({ directory: true, multiple: false });
                            if (typeof selected === "string") {
                              setFolder(selected);
                              await scanFolder(selected, cscMode);
                            }
                          }}><FolderOpen /></button>
                        </div>
                      </Field>
                      <p className="folder-help">Auto-selects BL / AP / CP / CSC / USERDATA packages from the folder.</p>
                      <div className="folder-line csc-row">
                        <span className="csc-label">CSC package:</span>
                        <div className="mini-segments">
                          <button className={cscMode === "home" ? "active" : ""} onClick={async () => { setCscMode("home"); await scanFolder(folder, "home"); }}>HOME_CSC · keep data</button>
                          <button className={cscMode === "wipe" ? "active danger" : ""} onClick={async () => { setCscMode("wipe"); await scanFolder(folder, "wipe"); }}>CSC · factory reset</button>
                        </div>
                      </div>
                      {cscMode === "wipe" && <div className="warning-row"><Warning /> Regular CSC will factory reset the device.</div>}
                    </Card>
                  ) : (
                    <Card className="package-slots-card">
                      <span className="package-card-title">Package slots · drop .tar / .tar.md5 files</span>
                      <div className="slot-grid">
                        {(["bl", "ap", "cp", "csc", "userdata"] as const).map((key) => (
                          <button key={key} className={`slot ${packages[key] ? "filled" : ""}`} onClick={() => pickPackage(key)}>
                            <span className="slot-badge">{key.toUpperCase()}</span>
                            <span><strong>{slotTitle(key)}</strong><code>{packages[key] ? basename(packages[key] as string) : key === "userdata" ? "not selected - optional" : "not selected"}</code></span>
                            {packages[key] ? <CheckCircle /> : <PlusCircle />}
                          </button>
                        ))}
                      </div>
                    </Card>
                  )}

                  <Card className="flash-options-card">
                    <span className="label">Options</span>
                    <div className="option-grid">
                      <CheckOption checked={flashOpts.noReboot} title="No auto-reboot" hint="Stay in download mode after flashing" onClick={() => setFlashOpts((o) => ({ ...o, noReboot: !o.noReboot }))} />
                      <CheckOption checked={flashOpts.wait} title="Wait for device" hint="Block until a device connects" onClick={() => setFlashOpts((o) => ({ ...o, wait: !o.wait }))} />
                      <CheckOption checked={flashOpts.skipMd5} title="Skip MD5 check" hint="Faster, but no integrity check" onClick={() => setFlashOpts((o) => ({ ...o, skipMd5: !o.skipMd5 }))} />
                      <CheckOption checked={flashOpts.skipSize} title="Skip size check" hint="Don't verify files fit partitions" onClick={() => setFlashOpts((o) => ({ ...o, skipSize: !o.skipSize }))} />
                      <CheckOption danger checked={flashOpts.repartition} title="Repartition" hint="Rewrites the partition table, requires a PIT and all packages. Risk of brick." onClick={() => setFlashOpts((o) => ({ ...o, repartition: !o.repartition }))} />
                    </div>
                  </Card>
                </div>

                <div className={`flash-dock ${flashState === "error" ? "danger-card" : ""}`}>
                  {flashState === "idle" ? (
                    <div className="dock-idle">
                      <span>
                        <Usb />
                        {flashMode === "folder"
                          ? folder ? "Firmware folder ready" : "Select a firmware folder"
                          : `${selectedPackages.length} package${selectedPackages.length === 1 ? "" : "s"} ready`}
                        {" · "}
                        {deviceConnected ? "device in download mode" : "will wait for a device"}
                      </span>
                      <button className={destructive ? "primary danger" : "primary"} disabled={flashMode === "folder" ? !folder : selectedPackages.length === 0} onClick={() => setFlashModal(true)}>
                        <Lightning weight="bold" /> Flash device
                      </button>
                    </div>
                  ) : (
                    <div className="dock-live">
                      <div className="dock-head">
                        <div className="row"><Usb className={flashState === "active" ? "blink" : ""} /><strong>{flashState === "done" ? "Flash complete" : flashState === "error" ? "Flash failed" : "Flashing partitions"}</strong></div>
                        <code>{Math.round(flashOverall.pct)}%</code>
                      </div>
                      <Progress value={flashOverall.pct} ok={flashState === "done"} />
                      <div className="row between flash-meta">
                        <span>{flashMessage}</span>
                        <code>{fmtBytes(flashOverall.bytes)} / {fmtBytes(flashOverall.total)}</code>
                      </div>
                      <div className="dock-parts">
                        {Object.entries(flashParts).map(([name, part]) => <PartitionRow key={name} name={name} part={part} />)}
                      </div>
                      {flashState !== "active" && (
                        <div className="dock-actions">
                          <button className="ghost" onClick={() => { setFlashState("idle"); setFlashParts({}); setFlashOverall({ bytes: 0, total: 0, pct: 0 }); setFlashMessage(""); }}>
                            Clear
                          </button>
                        </div>
                      )}
                    </div>
                  )}
                </div>
              </div>
            )}

            {view === "updates" && (
              <div className="stack narrow">
                <Card>
                  <div className="form-grid compact">
                    <Field label="Model"><input value={uModel} onChange={(e) => setUModel(e.target.value.toUpperCase())} /></Field>
                    <Field label="Region"><input value={uRegion} onChange={(e) => setURegion(e.target.value.toUpperCase())} /></Field>
                  </div>
                  <div className="action-row"><button className="primary" onClick={checkUpdates}><ArrowsClockwise className={updatesState === "active" ? "spin" : ""} /> Check</button></div>
                </Card>
                {updates && (
                  <Card rise>
                    <span className="label">Latest stable</span>
                    <code className="version-line">{updates.latest}</code>
                    <button className="ghost" onClick={() => { setDVersion(updates.latest); setView("download"); }}><DownloadSimple /> Download this version</button>
                    <div className="update-grid">
                      <VersionList title="Previous stable" rows={updates.previous} />
                      <VersionList title="Beta" rows={updates.beta} warn />
                    </div>
                  </Card>
                )}
              </div>
            )}

            {view === "device" && (
              <div className="stack wide">
                <div className="action-cards">
                  <ActionCard icon={<MagnifyingGlass />} title="Detect device" text="Scan download-mode backends" onClick={() => detectDevice(false)} />
                  <ActionCard icon={<Table />} title="Print PIT" text="Read partition table" onClick={printPit} />
                  <ActionCard icon={<ArrowUDownLeft />} title="Reboot to download" text="Ask connected device to reboot" onClick={rebootDownload} />
                </div>
                {pitRows.length > 0 && (
                  <Card rise>
                    <div className="row between"><strong>PIT entries</strong><button className="ghost" onClick={dumpPit}><DownloadSimple /> Dump to file</button></div>
                    <div className="pit-table">
                      <div className="pit-head"><span>ID</span><span>Partition</span><span>Flash file</span><span>Size</span></div>
                      {pitRows.map((row) => (
                        <div className="pit-row" key={`${row.id}-${row.partition}`}>
                          <code>{row.id}</code><strong>{row.partition}</strong><code>{row.flashFile}</code><code>{fmtBytes(row.size)}</code>
                        </div>
                      ))}
                    </div>
                  </Card>
                )}
              </div>
            )}

            {view === "verify" && (
              <div className="stack narrow">
                <button className="dropzone" onClick={addVerifyFiles}><FilePlus /><strong>Add .tar.md5 files</strong><span>MD5 footer verification</span></button>
                {verifyRows.map((row) => (
                  <div className="file-row" key={row.file}>
                    {row.status === "ok" ? <SealCheck weight="fill" /> : row.status === "checking" ? <Circle className="spin" /> : <FileArchive />}
                    <code>{basename(row.file)}</code>
                    <span>{row.message}</span>
                  </div>
                ))}
                {verifyRows.length > 0 && <button className="primary" onClick={verifyAll}><SealCheck weight="bold" /> Verify all</button>}
              </div>
            )}

            {view === "settings" && (
              <div className="stack narrow">
                <Card>
                  <span className="label">USB backend</span>
                  <div className="backend-list">
                    {["vcom", "nusb", "libusb"].map((item) => (
                      <button key={item} className={`backend-row ${backend === item ? "active" : ""}`} disabled={!appInfo.backends.includes(item)} onClick={() => setBackend(item)}>
                        <span><strong>{item}</strong><small>{backendCopy(item, appInfo.backends.includes(item))}</small></span>
                        {backend === item && <CheckCircle weight="fill" />}
                      </button>
                    ))}
                  </div>
                </Card>
                <Card>
                  <div className="setting-row"><span><strong>Verbose output</strong><small>Emit detailed backend logs</small></span><Switch on={verbose} onClick={() => setVerbose(!verbose)} /></div>
                  <div className="setting-row"><span><strong>Appearance</strong><small>{theme === "dark" ? "Dark" : "Light"} theme</small></span><button className="ghost" onClick={() => setTheme(theme === "dark" ? "light" : "dark")}>{theme === "dark" ? <Sun /> : <Moon />} Toggle</button></div>
                </Card>
              </div>
            )}
          </section>

          <footer className="command-bar">
            <TerminalWindow />
            <code>{command}</code>
            <button onClick={copyCommand}>{copied ? <Check /> : <Copy />}{copied ? "Copied" : "Copy"}</button>
          </footer>
        </main>
      </div>

      {flashModal && (
        <div className="modal-backdrop">
          <div className="modal">
            <div className="row">
              <div className={destructive ? "modal-icon danger" : "modal-icon"}>{destructive ? <WarningOctagon weight="fill" /> : <Lightning weight="fill" />}</div>
              <h2>{destructive ? "Confirm destructive flash" : "Confirm flash"}</h2>
            </div>
            <p>Review what will be written. Flashing the wrong firmware can brick your device.</p>
            <div className="modal-list">
              <ModalItem label="Mode" value={flashMode === "folder" ? "Firmware folder" : "Package files"} />
              <ModalItem label="CSC" value={flashMode === "folder" ? `${cscLabel(cscMode)} · ${cscMode === "wipe" ? "factory reset" : "keep data"}` : basename(packages.csc ?? "not selected")} />
              <ModalItem label="Packages" value={flashMode === "folder" ? `${selectedPackages.length} auto-selected` : `${selectedPackages.length} selected`} />
              <ModalItem label="Device" value={deviceConnected ? "connected" : "not connected · will wait"} />
            </div>
            {destructive && <div className="warning-row"><WarningOctagon />{flashOpts.repartition ? "Repartition rewrites the device partition table." : "Regular CSC performs a factory reset."}</div>}
            <div className="modal-actions">
              <button className="ghost" onClick={() => setFlashModal(false)}>Cancel</button>
              <button className={destructive ? "primary danger" : "primary"} onClick={confirmFlash}>{deviceConnected ? "Flash now" : "Wait for device & flash"}</button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

function NavGroup({ label, children }: { label: string; children: React.ReactNode }) {
  return <div className="nav-group"><div className="nav-label">{label}</div>{children}</div>;
}

function NavButton({ icon, label, active, onClick }: { icon: React.ReactNode; label: string; active: boolean; onClick: () => void }) {
  return <button className={`nav-button ${active ? "active" : ""}`} onClick={onClick}>{icon}<span>{label}</span></button>;
}

function StatusDot({ connected, busy }: { connected: boolean; busy: boolean }) {
  return <span className={`status-dot ${connected ? "ok" : ""} ${busy ? "busy" : ""}`} />;
}

function Card({ children, rise, className = "" }: { children: React.ReactNode; rise?: boolean; className?: string }) {
  return <div className={`card ${rise ? "rise" : ""} ${className}`}>{children}</div>;
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return <label className="field"><span>{label}</span>{children}</label>;
}

function Progress({ value, ok }: { value: number; ok?: boolean }) {
  return <div className="progress"><div className={ok ? "ok" : ""} style={{ width: `${Math.max(0, Math.min(value, 100))}%` }} /></div>;
}

function Stat({ label, value, accent }: { label: string; value: string; accent?: boolean }) {
  return <div className="stat"><span>{label}</span><code className={accent ? "accent" : ""}>{value}</code></div>;
}

function CheckOption({ checked, title, hint, danger, onClick }: { checked: boolean; title: string; hint: string; danger?: boolean; onClick: () => void }) {
  return <button className={`check-option ${checked ? "checked" : ""} ${danger ? "danger-option" : ""}`} onClick={onClick}><span className="fake-check">{checked && <Check weight="bold" />}</span><span><strong>{title}{danger && <Warning weight="fill" />}</strong><small>{hint}</small></span></button>;
}

function PartitionRow({ name, part }: { name: string; part: FlashPart }) {
  return <div className="partition-row"><CheckCircle weight={part.pct >= 100 ? "fill" : "regular"} /><code>{name}</code><span>{Math.round(part.pct)}%</span><Progress value={part.pct} ok={part.pct >= 100} /><code>{fmtBytes(part.bytes)} / {fmtBytes(part.total)}</code></div>;
}

function VersionList({ title, rows, warn }: { title: string; rows: string[]; warn?: boolean }) {
  return <div className="version-list"><span className={warn ? "label warn" : "label"}>{title}</span>{rows.slice(0, 8).map((row) => <code key={row}>{row}</code>)}</div>;
}

function ActionCard({ icon, title, text, onClick }: { icon: React.ReactNode; title: string; text: string; onClick: () => void }) {
  return <button className="action-card" onClick={onClick}>{icon}<strong>{title}</strong><span>{text}</span></button>;
}

function Switch({ on, onClick }: { on: boolean; onClick: () => void }) {
  return <button className={`switch ${on ? "on" : ""}`} onClick={onClick}><span /></button>;
}

function ModalItem({ label, value }: { label: string; value: string }) {
  return <div><span>{label}</span><code>{value}</code></div>;
}

function slotTitle(key: keyof FolderPackages) {
  return { bl: "Bootloader", ap: "Application", cp: "Modem", csc: "Consumer SW", userdata: "User data" }[key];
}

function backendCopy(backend: string, available: boolean) {
  if (!available) return "not enabled in this Windows-first build";
  if (backend === "vcom") return "Samsung USB serial driver";
  if (backend === "nusb") return "native USB backend";
  return "generic libusb backend";
}
