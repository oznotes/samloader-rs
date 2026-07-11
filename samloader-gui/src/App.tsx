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
  Pause,
  Play,
  PlusCircle,
  SealCheck,
  Sun,
  Table,
  TerminalWindow,
  Usb,
  Warning,
  WarningOctagon,
  X,
} from "@phosphor-icons/react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open, save } from "@tauri-apps/plugin-dialog";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

type View = "firmware" | "flash" | "device" | "verify" | "settings";
type Theme = "dark" | "light";
type FlashMode = "pkg" | "folder";
type CscMode = "home" | "wipe";
type RunState = "idle" | "active" | "done" | "error";
type DownloadState = "idle" | "preparing" | "downloading" | "paused" | "canceling" | "cancelled" | "done" | "error";
type FirmwareFilter = "all" | "stable" | "beta";
type FirmwareKind = "latest" | "stable" | "beta";

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
  defaultDownloadDir?: string;
};

type DownloadEvent = {
  status: string;
  filename?: string | null;
  path?: string | null;
  bytes?: number;
  total?: number;
  speedBps?: number;
  message?: string | null;
};

type FirmwareResults = {
  latest: string;
  previous: string[];
  beta: string[];
};

type FirmwareRow = {
  version: string;
  kind: FirmwareKind;
};

type DownloadPlan = {
  model: string;
  region: string;
  version: string;
  sourceFilename: string;
  filename: string;
  path: string;
  partPath: string;
  size: number;
  availableSpace: number | null;
  enoughSpace: boolean;
  conflict: boolean;
  partConflict: boolean;
};

type AndroidDevice = {
  status: string;
  connected: boolean;
  model?: string | null;
  region?: string | null;
  serial?: string | null;
  message: string;
};

type Preferences = {
  theme: Theme;
  backend: string;
  verbose: boolean;
  model: string;
  region: string;
  outDir: string;
  threads: number;
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

const fmtDuration = (seconds: number) => {
  if (!Number.isFinite(seconds) || seconds <= 0) return "--";
  const rounded = Math.ceil(seconds);
  const hours = Math.floor(rounded / 3600);
  const minutes = Math.floor((rounded % 3600) / 60);
  const secs = rounded % 60;
  if (hours) return `${hours}h ${minutes}m`;
  if (minutes) return `${minutes}m ${secs}s`;
  return `${secs}s`;
};

const errorMessage = (error: unknown) => {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return "The operation could not be completed.";
};

const readPreferences = (): Partial<Preferences> => {
  if (typeof window === "undefined") return {};
  try {
    return JSON.parse(window.localStorage.getItem("samloader.preferences.v2") ?? "{}") as Partial<Preferences>;
  } catch {
    return {};
  }
};

const KNOWN_CSC_CODES = [
  ["XAA", "United States · unlocked"], ["EUX", "Europe · open market"], ["EUY", "Europe · open market"],
  ["BTU", "United Kingdom · open market"], ["DBT", "Germany · open market"], ["XEF", "France · open market"],
  ["ITV", "Italy · open market"], ["PHE", "Spain · open market"], ["XEO", "Poland · open market"],
  ["PHN", "Netherlands · open market"], ["NEE", "Nordic countries"], ["AUT", "Switzerland · open market"],
  ["ATO", "Austria · open market"], ["TPH", "Portugal · open market"], ["SEE", "South East Europe"],
  ["XSG", "United Arab Emirates"], ["KSA", "Saudi Arabia"], ["INS", "India"], ["THL", "Thailand"],
  ["XSP", "Singapore"], ["XME", "Malaysia"], ["XTC", "Philippines"], ["XSA", "Australia · open market"],
  ["TEL", "Australia · Telstra"], ["NZC", "New Zealand"], ["TGY", "Hong Kong"], ["BRI", "Taiwan"],
  ["CHC", "China · open market"], ["KOO", "South Korea · open market"], ["ZTO", "Brazil · open market"],
  ["MXO", "Mexico · open market"], ["COO", "Colombia · open market"], ["ARO", "Argentina · open market"],
] as const;

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
  const saved = useRef(readPreferences()).current;
  const [appInfo, setAppInfo] = useState<AppInfo>({
    version: "2.0.0",
    defaultBackend: "vcom",
    backends: ["vcom", "nusb"],
  });
  const [theme, setTheme] = useState<Theme>(saved.theme === "light" ? "light" : "dark");
  const [view, setView] = useState<View>("firmware");
  const [backend, setBackend] = useState(saved.backend ?? "vcom");
  const [verbose, setVerbose] = useState(saved.verbose ?? false);
  const [copied, setCopied] = useState(false);

  const [deviceConnected, setDeviceConnected] = useState(false);
  const [deviceBusy, setDeviceBusy] = useState(false);
  const [pitRows, setPitRows] = useState<PitRow[]>([]);
  const [deviceMessage, setDeviceMessage] = useState("Odin / download mode");
  const [deviceError, setDeviceError] = useState("");
  const deviceStatusInFlight = useRef(false);

  const [dModel, setDModel] = useState(saved.model ?? "");
  const [dRegion, setDRegion] = useState(saved.region ?? "");
  const [dVersion, setDVersion] = useState("");
  const [dOut, setDOut] = useState(saved.outDir ?? "");
  const [dThreads, setDThreads] = useState(Math.min(16, Math.max(1, saved.threads ?? 8)));
  const [manualConnections, setManualConnections] = useState(false);
  const [downloadState, setDownloadState] = useState<DownloadState>("idle");
  const [downloadEvent, setDownloadEvent] = useState<DownloadEvent | null>(null);
  const [downloadError, setDownloadError] = useState("");
  const [downloadPlan, setDownloadPlan] = useState<DownloadPlan | null>(null);
  const [allowReplace, setAllowReplace] = useState(false);
  const [preparingVersion, setPreparingVersion] = useState("");
  const [downloadControlBusy, setDownloadControlBusy] = useState(false);
  const [lastFirmware, setLastFirmware] = useState<FirmwareRow | null>(null);

  const [firmwareState, setFirmwareState] = useState<RunState>("idle");
  const [firmwareError, setFirmwareError] = useState("");
  const [firmwareResults, setFirmwareResults] = useState<FirmwareResults | null>(null);
  const [firmwareFilter, setFirmwareFilter] = useState<FirmwareFilter>("all");
  const [firmwareQuery, setFirmwareQuery] = useState("");
  const [androidDevice, setAndroidDevice] = useState<AndroidDevice | null>(null);
  const [androidDetecting, setAndroidDetecting] = useState(false);

  const [flashMode, setFlashMode] = useState<FlashMode>("folder");
  const [folder, setFolder] = useState("");
  const [folderScanBusy, setFolderScanBusy] = useState(false);
  const folderScanGeneration = useRef(0);
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
  const [flashError, setFlashError] = useState("");
  const [flashParts, setFlashParts] = useState<Record<string, FlashPart>>({});
  const [flashOverall, setFlashOverall] = useState({ bytes: 0, total: 0, pct: 0 });

  const [verifyRows, setVerifyRows] = useState<VerifyRow[]>([]);
  const [verifyError, setVerifyError] = useState("");
  const downloadCancelRef = useRef<HTMLButtonElement>(null);
  const flashCancelRef = useRef<HTMLButtonElement>(null);

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
        setBackend((current) => info.backends.includes(current) ? current : info.defaultBackend);
      })
      .catch(() => undefined);

    const unlistenDownload = listen<DownloadEvent>("download-progress", (event) => {
      const payload = event.payload;
      setDownloadEvent((current) => ({ ...current, ...payload }));
      if (payload.status === "done") setDownloadState("done");
      else if (payload.status === "error") {
        setDownloadState("error");
        setDownloadError(payload.message ?? "Download failed.");
      } else if (payload.status === "paused") setDownloadState("paused");
      else if (payload.status === "canceling") setDownloadState("canceling");
      else if (payload.status === "cancelled") setDownloadState("cancelled");
      else if (["started", "downloading", "resumed"].includes(payload.status)) setDownloadState("downloading");
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
    try {
      window.localStorage.setItem("samloader.preferences.v2", JSON.stringify({
        theme, backend, verbose, model: dModel, region: dRegion, outDir: dOut, threads: dThreads,
      } satisfies Preferences));
    } catch {
      // Preferences are optional; a restricted WebView must not break the app.
    }
  }, [backend, dModel, dOut, dRegion, dThreads, theme, verbose]);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        if (downloadPlan) setDownloadPlan(null);
        else if (flashModal) setFlashModal(false);
        return;
      }
      if (event.key !== "Tab" || (!downloadPlan && !flashModal)) return;

      const modal = document.querySelector<HTMLElement>(downloadPlan ? ".download-review" : ".modal");
      const focusable = modal
        ? Array.from(modal.querySelectorAll<HTMLElement>('button:not([disabled]), input:not([disabled]), [href], [tabindex]:not([tabindex="-1"])'))
        : [];
      if (!focusable.length) return;
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [downloadPlan, flashModal]);

  useEffect(() => {
    if (downloadPlan) window.setTimeout(() => downloadCancelRef.current?.focus(), 0);
  }, [downloadPlan]);

  useEffect(() => {
    if (flashModal) window.setTimeout(() => flashCancelRef.current?.focus(), 0);
  }, [flashModal]);

  useEffect(() => {
    if (!hasTauriRuntime() || flashState === "active") return;

    void refreshDeviceStatus(false, false);
    const interval = window.setInterval(() => {
      void refreshDeviceStatus(false, false);
    }, 2500);

    return () => window.clearInterval(interval);
  }, [flashState, refreshDeviceStatus]);

  const selectedPackages = packageList(packages);
  const firmwareRows = useMemo<FirmwareRow[]>(() => {
    if (!firmwareResults) return [];
    const seen = new Set<string>();
    const rows: FirmwareRow[] = [];
    const add = (version: string, kind: FirmwareKind) => {
      const clean = version.trim();
      if (!clean || seen.has(clean)) return;
      seen.add(clean);
      rows.push({ version: clean, kind });
    };
    add(firmwareResults.latest, "latest");
    firmwareResults.previous.forEach((version) => add(version, "stable"));
    firmwareResults.beta.forEach((version) => add(version, "beta"));
    return rows;
  }, [firmwareResults]);
  const visibleFirmwareRows = firmwareRows.filter((row) => {
    const channelMatches = firmwareFilter === "all" || (firmwareFilter === "stable" ? row.kind !== "beta" : row.kind === "beta");
    return channelMatches && row.version.toLowerCase().includes(firmwareQuery.trim().toLowerCase());
  });
  const downloadLocked = ["preparing", "downloading", "paused", "canceling"].includes(downloadState);
  const connectionThreads = manualConnections ? dThreads : 8;
  const command = useMemo(() => {
    const globals = `${backend ? ` --usb-backend ${backend}` : ""}${verbose ? " --verbose" : ""}`;
    if (view === "firmware") {
      if (!dVersion) return `samloader${globals} check-update -m ${dModel || "MODEL"} -r ${dRegion || "CSC"} --all`;
      return `samloader${globals} download -m ${dModel || "MODEL"} -r ${dRegion || "CSC"} -v ${quote(dVersion)} -j ${connectionThreads}${dOut ? ` -d ${quote(dOut)}` : ""}`;
    }
    if (view === "flash") {
      const opts = `${flashOpts.wait ? " --wait" : ""}${flashOpts.noReboot ? " --no-reboot" : ""}${flashOpts.skipMd5 ? " --skip-md5" : ""}${flashOpts.skipSize ? " --skip-size-check" : ""}${flashOpts.repartition ? " --repartition" : ""}`;
      if (flashMode === "folder") {
        return `samloader${globals} flash${opts} --folder ${quote(folder || "<folder>")}${cscMode === "wipe" ? " --csc-mode wipe" : ""}`;
      }
      return `samloader${globals} flash${opts}${packages.bl ? ` -b ${quote(packages.bl)}` : ""}${packages.ap ? ` -a ${quote(packages.ap)}` : ""}${packages.cp ? ` -c ${quote(packages.cp)}` : ""}${packages.csc ? ` -s ${quote(packages.csc)}` : ""}${packages.userdata ? ` -u ${quote(packages.userdata)}` : ""}`;
    }
    if (view === "device") return pitRows.length ? `samloader${globals} print-pit` : `samloader${globals} detect`;
    if (view === "verify") return `samloader verify-md5 ${verifyRows.map((row) => quote(row.file)).join(" ") || "<files>"}`;
    return `samloader${globals}`;
  }, [backend, connectionThreads, cscMode, dModel, dOut, dRegion, dVersion, flashMode, flashOpts, folder, packages, pitRows.length, verbose, verifyRows, view]);

  const title = {
    firmware: ["Firmware Explorer", "Find every known stable and beta build, then download it safely."],
    flash: ["Flash firmware", "Write packages or an extracted folder to your device."],
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
    setFirmwareError("");
    try {
      const selected = await open({ directory: true, multiple: false });
      if (typeof selected === "string") onPick(selected);
    } catch (error) {
      setFirmwareError(errorMessage(error));
    }
  }

  async function pickPackage(key: keyof FolderPackages) {
    setFlashError("");
    try {
      const selected = await open({
        multiple: false,
        filters: [{ name: "Odin package", extensions: ["tar", "md5"] }],
      });
      if (typeof selected === "string") {
        setPackages((current) => ({ ...current, [key]: selected }));
      }
    } catch (error) {
      setFlashError(errorMessage(error));
    }
  }

  async function scanFolder(path = folder, mode = cscMode) {
    if (!path.trim()) return;
    const generation = ++folderScanGeneration.current;
    setFolderScanBusy(true);
    setFlashError("");
    setPackages({});
    try {
      const result = await invoke<FolderPackages>("scan_firmware_folder", { folder: path, cscMode: mode });
      if (generation !== folderScanGeneration.current) return;
      setPackages(result);
      if (packageList(result).length === 0) setFlashError("No BL, AP, CP, CSC, or USERDATA packages were found in this folder.");
    } catch (error) {
      if (generation !== folderScanGeneration.current) return;
      setFlashError(errorMessage(error));
    } finally {
      if (generation === folderScanGeneration.current) setFolderScanBusy(false);
    }
  }

  function validateFirmwareIdentity(model = dModel, region = dRegion) {
    const cleanModel = model.trim().toUpperCase();
    const cleanRegion = region.trim().toUpperCase();
    if (!/^[A-Z0-9_.-]{2,48}$/.test(cleanModel)) return "Enter a valid 2–48 character device model, for example SM-S931U1.";
    if (!/^[A-Z0-9]{3}$/.test(cleanRegion)) return "Enter a three-character sales code (CSC), for example XAA.";
    return "";
  }

  async function searchFirmwares(model = dModel, region = dRegion) {
    const cleanModel = model.trim().toUpperCase();
    const cleanRegion = region.trim().toUpperCase();
    const validation = validateFirmwareIdentity(cleanModel, cleanRegion);
    if (validation) {
      setFirmwareError(validation);
      return;
    }
    setDModel(cleanModel);
    setDRegion(cleanRegion);
    setDVersion("");
    setFirmwareError("");
    setFirmwareResults(null);
    setFirmwareFilter("all");
    setFirmwareQuery("");
    setFirmwareState("active");
    try {
      const result = await invoke<FirmwareResults>("check_updates", { model: cleanModel, region: cleanRegion });
      setFirmwareResults({
        latest: result.latest ?? "",
        previous: Array.isArray(result.previous) ? result.previous : [],
        beta: Array.isArray(result.beta) ? result.beta : [],
      });
      setFirmwareState("done");
    } catch (error) {
      setFirmwareState("error");
      setFirmwareError(errorMessage(error));
    }
  }

  async function useConnectedDevice() {
    if (androidDetecting) return;
    setAndroidDetecting(true);
    setFirmwareError("");
    try {
      const result = await invoke<AndroidDevice>("detect_android_device");
      setAndroidDevice(result);
      if (!result.connected) {
        setFirmwareError(result.message || "No authorized Android device was found. Connect it with USB debugging enabled.");
        return;
      }
      const model = result.model?.trim().toUpperCase() ?? "";
      const region = result.region?.trim().toUpperCase() ?? "";
      if (model) setDModel(model);
      if (region) setDRegion(region);
      if (!model || !region) {
        setFirmwareError(`${result.message || "Device detected."} ${!model && !region ? "Model and CSC could not be read." : !model ? "Model could not be read." : "CSC could not be read."} Enter the missing value to search.`);
        return;
      }
      await searchFirmwares(model, region);
    } catch (error) {
      setFirmwareError(errorMessage(error));
    } finally {
      setAndroidDetecting(false);
    }
  }

  async function prepareDownload(row: FirmwareRow) {
    if (downloadLocked || preparingVersion) return;
    const validation = validateFirmwareIdentity();
    if (validation) {
      setFirmwareError(validation);
      return;
    }
    setDVersion(row.version);
    setLastFirmware(row);
    setPreparingVersion(row.version);
    setDownloadEvent(null);
    setDownloadError("");
    setDownloadState("preparing");
    setAllowReplace(false);
    try {
      const plan = await invoke<DownloadPlan>("prepare_download", {
        req: {
          model: dModel.trim().toUpperCase(), region: dRegion.trim().toUpperCase(), version: row.version,
          outDir: dOut.trim() || null, outFile: null, threads: connectionThreads, verbose, overwrite: false,
        },
      });
      setDownloadPlan(plan);
      setDownloadState("idle");
    } catch (error) {
      setDownloadState("error");
      setDownloadError(`${row.kind === "beta" ? "This beta may no longer be available from Samsung. " : ""}${errorMessage(error)}`);
    } finally {
      setPreparingVersion("");
    }
  }

  async function startDownload() {
    if (!downloadPlan || downloadLocked) return;
    const plan = downloadPlan;
    setDownloadPlan(null);
    setDownloadError("");
    setDownloadState("downloading");
    setDownloadEvent({ status: "started", filename: plan.filename, path: plan.path, bytes: 0, total: plan.size, speedBps: 0, message: "Starting secure download" });
    try {
      await invoke("start_download", {
        req: {
          model: plan.model, region: plan.region, version: plan.version,
          outDir: dOut.trim() || null, outFile: null, threads: connectionThreads, verbose,
          overwrite: (plan.conflict || plan.partConflict) && allowReplace,
        },
      });
    } catch (error) {
      setDownloadState("error");
      setDownloadError(errorMessage(error));
    }
  }

  async function controlDownload(command: "pause_download" | "resume_download" | "cancel_download") {
    if (downloadControlBusy) return;
    const previous = downloadState;
    setDownloadControlBusy(true);
    setDownloadError("");
    if (command === "cancel_download") setDownloadState("canceling");
    try {
      await invoke(command);
      if (command === "pause_download") setDownloadState("paused");
      else if (command === "resume_download") setDownloadState("downloading");
      // Cancellation remains in progress until the worker confirms cleanup with a terminal event.
    } catch (error) {
      setDownloadState(previous);
      setDownloadError(errorMessage(error));
    } finally {
      setDownloadControlBusy(false);
    }
  }

  async function detectDevice(wait = false) {
    await refreshDeviceStatus(wait, true);
  }

  async function printPit() {
    setDeviceBusy(true);
    setDeviceError("");
    try {
      const rows = await invoke<PitRow[]>("read_device_pit", { backend, wait: true, verbose });
      setPitRows(rows);
      setDeviceConnected(true);
      setDeviceMessage("PIT loaded from device");
    } catch (error) {
      setDeviceError(errorMessage(error));
    } finally {
      setDeviceBusy(false);
    }
  }

  async function dumpPit() {
    setDeviceError("");
    try {
      const output = await save({ filters: [{ name: "PIT", extensions: ["pit"] }] });
      if (output) await invoke("dump_device_pit", { backend, wait: true, verbose, output });
    } catch (error) {
      setDeviceError(errorMessage(error));
    }
  }

  async function rebootDownload() {
    setDeviceBusy(true);
    setDeviceError("");
    try {
      await invoke("reboot_to_download", { backend });
      setDeviceConnected(true);
      setDeviceMessage("Reboot command sent");
    } catch (error) {
      setDeviceError(errorMessage(error));
    } finally {
      setDeviceBusy(false);
    }
  }

  async function addVerifyFiles() {
    setVerifyError("");
    try {
      const selected = await open({
        multiple: true,
        filters: [{ name: "MD5 package", extensions: ["md5"] }],
      });
      const files = Array.isArray(selected) ? selected : typeof selected === "string" ? [selected] : [];
      setVerifyRows((rows) => {
        const known = new Set(rows.map((row) => row.file));
        return [...rows, ...files.filter((file) => !known.has(file)).map((file) => ({ file, status: "pending" as const, message: "Ready to verify" }))];
      });
    } catch (error) {
      setVerifyError(errorMessage(error));
    }
  }

  async function verifyAll() {
    if (!verifyRows.length || verifyRows.some((row) => row.status === "checking")) return;
    setVerifyError("");
    setVerifyRows((rows) => rows.map((row) => ({ ...row, status: "checking", message: "Verifying..." })));
    try {
      const result = await invoke<Array<{ file: string; ok: boolean; message: string }>>("verify_md5_files", {
        files: verifyRows.map((row) => row.file),
      });
      setVerifyRows(result.map((row) => ({ file: row.file, status: row.ok ? "ok" : "error", message: row.message })));
    } catch (error) {
      const message = errorMessage(error);
      setVerifyError(message);
      setVerifyRows((rows) => rows.map((row) => ({ ...row, status: "error", message })));
    }
  }

  async function confirmFlash() {
    setFlashModal(false);
    setFlashState("active");
    setFlashParts({});
    setFlashOverall({ bytes: 0, total: 0, pct: 0 });
    setFlashMessage(deviceConnected ? "Preparing flash" : "Waiting for device");
    setFlashError("");
    try {
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
    } catch (error) {
      const message = errorMessage(error);
      setFlashState("error");
      setFlashError(message);
      setFlashMessage(message);
    }
  }

  const destructive = flashOpts.repartition || (flashMode === "folder" && cscMode === "wipe");
  const downloadedBytes = downloadEvent?.bytes ?? 0;
  const downloadTotal = downloadEvent?.total ?? 0;
  const downloadSpeed = downloadEvent?.speedBps ?? 0;
  const downloadPct = downloadTotal ? Math.min(100, (downloadedBytes / downloadTotal) * 100) : 0;
  const downloadEta = downloadSpeed > 0 && downloadTotal > downloadedBytes
    ? fmtDuration((downloadTotal - downloadedBytes) / downloadSpeed)
    : "--";
  const anyDeviceConnected = deviceConnected || Boolean(androidDevice?.connected);
  const deviceStatusTitle = deviceConnected
    ? "Download mode device"
    : androidDevice?.connected
      ? "Android device"
    : deviceBusy
      ? "Detecting..."
      : "No download-mode device";
  const sidebarDeviceMessage = deviceConnected ? deviceMessage : androidDevice?.connected
    ? `${androidDevice.model ?? "Samsung device"}${androidDevice.serial ? ` · ${androidDevice.serial}` : ""}`
    : deviceMessage;

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
            <NavButton icon={<MagnifyingGlass />} label="Firmware Explorer" active={view === "firmware"} onClick={() => setView("firmware")} />
            <NavButton icon={<Lightning />} label="Flash" active={view === "flash"} onClick={() => setView("flash")} />
          </NavGroup>
          <NavGroup label="Device tools">
            <NavButton icon={<DeviceMobile />} label="Device & PIT" active={view === "device"} onClick={() => setView("device")} />
            <NavButton icon={<SealCheck />} label="Verify MD5" active={view === "verify"} onClick={() => setView("verify")} />
          </NavGroup>
          <div className="sidebar-spacer" />
          <div className="device-card">
            <StatusDot connected={anyDeviceConnected} busy={deviceBusy || androidDetecting} />
            <div>
              <strong>{deviceStatusTitle}</strong>
              <span>{sidebarDeviceMessage}</span>
            </div>
          </div>
          <div className="sidebar-actions">
            <button
              onClick={() => setView("settings")}
              className="ghost stretch"
              aria-label="Settings"
              title="Settings"
            >
              <GearSix /><span>Settings</span>
            </button>
            <button onClick={() => setTheme(theme === "dark" ? "light" : "dark")} className="icon-button" title="Toggle theme" aria-label="Toggle color theme">
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
          </header>

          <section className={`content ${view === "flash" ? "flash-content" : ""}`}>
            {view === "firmware" && (
              <div className="stack wide firmware-stack">
                <Card className="firmware-search-card">
                  <div className="firmware-search-grid">
                    <Field label="Device model">
                      <input disabled={downloadLocked || Boolean(preparingVersion)} value={dModel} onChange={(e) => { setDModel(e.target.value.toUpperCase()); setDVersion(""); }} placeholder="SM-S931U1" autoComplete="off" aria-describedby="model-help" />
                    </Field>
                    <Field label="Sales code (CSC)">
                      <input disabled={downloadLocked || Boolean(preparingVersion)} value={dRegion} onChange={(e) => { setDRegion(e.target.value.toUpperCase()); setDVersion(""); }} placeholder="XAA" list="known-csc-codes" autoComplete="off" aria-describedby="csc-help" />
                      <datalist id="known-csc-codes">{KNOWN_CSC_CODES.map(([code, label]) => <option key={code} value={code}>{label}</option>)}</datalist>
                    </Field>
                    <button className="ghost detect-android" disabled={androidDetecting || firmwareState === "active" || downloadLocked || Boolean(preparingVersion)} onClick={useConnectedDevice}>
                      <DeviceMobile /> {androidDetecting ? "Reading device..." : "Use connected device"}
                    </button>
                  </div>
                  <div className="field-help-row">
                    <span id="model-help">Printed in Settings → About phone.</span>
                    <span id="csc-help"><strong>Known CSC codes</strong> are suggestions, not a complete compatibility list. Custom codes are accepted and validated by firmware search.</span>
                  </div>
                  {androidDevice?.connected && <div className="inline-info"><CheckCircle weight="fill" /> {androidDevice.message} {androidDevice.serial && <code>{androidDevice.serial}</code>}</div>}
                  {firmwareError && <div className="inline-error" role="alert"><WarningOctagon /> <span>{firmwareError}</span></div>}
                  <div className="download-options">
                    <Field label="Output folder">
                      <div className="input-action">
                        <input disabled={downloadLocked || Boolean(preparingVersion)} value={dOut} onChange={(e) => setDOut(e.target.value)} placeholder={appInfo.defaultDownloadDir || "System Downloads folder"} />
                        <button disabled={downloadLocked || Boolean(preparingVersion)} aria-label="Choose output folder" onClick={() => pickDirectory(setDOut)}><FolderOpen /></button>
                      </div>
                    </Field>
                    <div className="threads-control" aria-label="Connection strategy">
                      <div className="row between"><span className="label">Connections</span><code className="accent">{manualConnections ? dThreads : "Auto · 8"}</code></div>
                      <div className="connection-choice">
                        <button disabled={downloadLocked || Boolean(preparingVersion)} className={!manualConnections ? "active" : ""} aria-pressed={!manualConnections} onClick={() => setManualConnections(false)}>Auto</button>
                        <button disabled={downloadLocked || Boolean(preparingVersion)} className={manualConnections ? "active" : ""} aria-pressed={manualConnections} onClick={() => setManualConnections(true)}>Manual</button>
                      </div>
                      {manualConnections ? <input disabled={downloadLocked || Boolean(preparingVersion)} aria-label="Parallel download connections" className="range" type="range" min={1} max={16} value={dThreads} onChange={(e) => setDThreads(Number(e.target.value))} /> : <small>Starts at 8 and reduces if the server throttles.</small>}
                    </div>
                    <button className="primary search-button" disabled={firmwareState === "active" || Boolean(preparingVersion) || downloadLocked} onClick={() => searchFirmwares()}>
                      <ArrowsClockwise className={firmwareState === "active" ? "spin" : ""} /> {firmwareState === "active" ? "Searching..." : "Search firmwares"}
                    </button>
                  </div>
                </Card>

                {firmwareResults && (
                  <Card rise className="firmware-results-card">
                    <div className="firmware-results-head">
                      <div><strong>{firmwareRows.length} firmware{firmwareRows.length === 1 ? "" : "s"}</strong><p>{dModel} · {dRegion} · all known results</p></div>
                      <div className="firmware-result-tools">
                        <label className="version-search"><MagnifyingGlass /><input value={firmwareQuery} onChange={(event) => setFirmwareQuery(event.target.value)} placeholder="Filter version" aria-label="Filter firmware by version" /></label>
                        <div className="filter-segments" aria-label="Filter firmware versions">
                          {(["all", "stable", "beta"] as const).map((filter) => <button key={filter} aria-pressed={firmwareFilter === filter} className={firmwareFilter === filter ? "active" : ""} onClick={() => setFirmwareFilter(filter)}>{filter}</button>)}
                        </div>
                      </div>
                    </div>
                    {firmwareResults.beta.length > 0 && <div className="beta-note"><Warning /> Beta records can remain listed after Samsung removes the file. Availability is checked before download.</div>}
                    <div className="firmware-table" role="table" aria-label="Available firmware versions">
                      <div className="firmware-table-head" role="row"><span>Version</span><span>Channel</span><span>Availability</span><span /></div>
                      {visibleFirmwareRows.map((row) => (
                        <div className="firmware-row" role="row" key={`${row.kind}-${row.version}`}>
                          <code title={row.version}>{row.version}</code>
                          <span className={`firmware-badge ${row.kind}`}>{row.kind === "latest" ? "Latest stable" : row.kind}</span>
                          <span className="availability">Checked during preflight</span>
                          <button className="ghost" disabled={downloadLocked || Boolean(preparingVersion)} onClick={() => prepareDownload(row)}>
                            {preparingVersion === row.version ? <ArrowsClockwise className="spin" /> : <DownloadSimple />} {preparingVersion === row.version ? "Checking..." : "Download"}
                          </button>
                        </div>
                      ))}
                    </div>
                    {visibleFirmwareRows.length === 0 && <div className="empty-state">No firmware matches this channel and version search.</div>}
                  </Card>
                )}

                {(downloadState !== "idle" || downloadError) && (
                  <Card rise className={`download-card ${downloadState === "error" ? "danger-card" : ""}`}>
                    <div className="row between download-head">
                      <div className="row">
                        <StatusDot connected={downloadState === "done"} busy={["preparing", "downloading", "canceling"].includes(downloadState)} />
                        <div><code>{downloadEvent?.filename ?? (dVersion || "Firmware download")}</code><p aria-live="polite">{downloadError || downloadEvent?.message || downloadStatusCopy(downloadState)}</p></div>
                      </div>
                      <strong className="big-pct">{Math.round(downloadPct)}%</strong>
                    </div>
                    <Progress value={downloadPct} ok={downloadState === "done"} label={`Download ${Math.round(downloadPct)} percent`} />
                    <div className="stats download-stats">
                      <Stat label="Downloaded" value={fmtBytes(downloadedBytes)} />
                      <Stat label="Total" value={fmtBytes(downloadTotal)} />
                      <Stat label="Speed" value={downloadSpeed ? `${fmtBytes(downloadSpeed)}/s` : "--"} accent />
                      <Stat label="ETA" value={downloadEta} />
                      <Stat label="Output" value={downloadEvent?.path ? basename(downloadEvent.path) : "--"} />
                    </div>
                    <div className="download-actions">
                      {downloadState === "downloading" && <button className="ghost" disabled={downloadControlBusy} onClick={() => controlDownload("pause_download")}><Pause /> Pause</button>}
                      {downloadState === "paused" && <button className="primary compact-button" disabled={downloadControlBusy} onClick={() => controlDownload("resume_download")}><Play /> Resume</button>}
                      {["downloading", "paused"].includes(downloadState) && <button className="ghost danger-outline" disabled={downloadControlBusy} onClick={() => controlDownload("cancel_download")}><X /> Cancel</button>}
                      {["error", "cancelled"].includes(downloadState) && lastFirmware && <button className="primary compact-button" onClick={() => prepareDownload(lastFirmware)}><ArrowsClockwise /> Retry</button>}
                      {["done", "error", "cancelled"].includes(downloadState) && <button className="ghost" onClick={() => { setDownloadState("idle"); setDownloadEvent(null); setDownloadError(""); setDVersion(""); }}>Clear</button>}
                    </div>
                  </Card>
                )}
              </div>
            )}

            {view === "flash" && (
              <div className="stack wide flash-stack">
                <div className="flash-config">
                  <div className="segments">
                    <button className={flashMode === "pkg" ? "active" : ""} onClick={() => { folderScanGeneration.current += 1; setFolderScanBusy(false); setFlashMode("pkg"); setPackages({}); setFlashError(""); }}>Package files</button>
                    <button className={flashMode === "folder" ? "active" : ""} onClick={() => { folderScanGeneration.current += 1; setFolderScanBusy(false); setFlashMode("folder"); setPackages({}); setFlashError(""); }}>Firmware folder</button>
                  </div>

                  {flashMode === "folder" ? (
                    <Card className="flash-folder-card">
                      <Field label="Extracted firmware folder">
                        <div className="input-action">
                          <input value={folder} onChange={(e) => { folderScanGeneration.current += 1; setFolderScanBusy(false); setFolder(e.target.value); setPackages({}); setFlashError(""); }} placeholder="D:\\SAMFW..." />
                          <button aria-label="Scan typed firmware folder" disabled={!folder.trim() || folderScanBusy} onClick={() => scanFolder()}><MagnifyingGlass className={folderScanBusy ? "spin" : ""} /></button>
                          <button aria-label="Choose firmware folder" disabled={folderScanBusy} onClick={async () => {
                            setFlashError("");
                            try {
                              const selected = await open({ directory: true, multiple: false });
                              if (typeof selected === "string") {
                                setFolder(selected);
                                setPackages({});
                                await scanFolder(selected, cscMode);
                              }
                            } catch (error) {
                              setFlashError(errorMessage(error));
                            }
                          }}><FolderOpen /></button>
                        </div>
                      </Field>
                      <p className="folder-help">Auto-selects BL / AP / CP / CSC / USERDATA packages from the folder.</p>
                      <div className="folder-line csc-row">
                        <span className="csc-label">CSC package:</span>
                        <div className="mini-segments">
                          <button disabled={folderScanBusy} className={cscMode === "home" ? "active" : ""} onClick={async () => { setCscMode("home"); await scanFolder(folder, "home"); }}>HOME_CSC · keep data</button>
                          <button disabled={folderScanBusy} className={cscMode === "wipe" ? "active danger" : ""} onClick={async () => { setCscMode("wipe"); await scanFolder(folder, "wipe"); }}>CSC · factory reset</button>
                        </div>
                      </div>
                      {cscMode === "wipe" && <div className="warning-row"><Warning /> Regular CSC will factory reset the device.</div>}
                    </Card>
                  ) : (
                    <Card className="package-slots-card">
                      <span className="package-card-title">Package slots · choose .tar / .tar.md5 files</span>
                      <div className="slot-grid">
                        {(["bl", "ap", "cp", "csc", "userdata"] as const).map((key) => (
                          <div className="slot-wrap" key={key}>
                            <button className={`slot ${packages[key] ? "filled" : ""}`} onClick={() => pickPackage(key)}>
                              <span className="slot-badge">{key.toUpperCase()}</span>
                              <span><strong>{slotTitle(key)}</strong><code>{packages[key] ? basename(packages[key] as string) : key === "userdata" ? "not selected - optional" : "not selected"}</code></span>
                              {packages[key] ? <CheckCircle /> : <PlusCircle />}
                            </button>
                            {packages[key] && <button className="slot-remove" aria-label={`Clear ${key.toUpperCase()} package`} onClick={() => setPackages((current) => ({ ...current, [key]: null }))}><X /></button>}
                          </div>
                        ))}
                      </div>
                    </Card>
                  )}

                  <Card className="flash-options-card">
                    <span className="label">Flash behavior</span>
                    <div className="option-grid">
                      <CheckOption checked={flashOpts.noReboot} title="No auto-reboot" hint="Stay in download mode after flashing" onClick={() => setFlashOpts((o) => ({ ...o, noReboot: !o.noReboot }))} />
                      <CheckOption checked={flashOpts.wait} title="Wait for device" hint="Block until a device connects" onClick={() => setFlashOpts((o) => ({ ...o, wait: !o.wait }))} />
                    </div>
                    <details className="advanced-options" open={flashOpts.skipMd5 || flashOpts.skipSize || flashOpts.repartition || undefined}>
                      <summary>Advanced and risky options</summary>
                      <p>Keep these checks enabled unless you understand the failure mode.</p>
                      <div className="option-grid">
                        <CheckOption checked={flashOpts.skipMd5} title="Skip MD5 check" hint="Disables package integrity protection" onClick={() => setFlashOpts((o) => ({ ...o, skipMd5: !o.skipMd5 }))} />
                        <CheckOption checked={flashOpts.skipSize} title="Skip size check" hint="Disables partition fit protection" onClick={() => setFlashOpts((o) => ({ ...o, skipSize: !o.skipSize }))} />
                        <CheckOption danger checked={flashOpts.repartition} title="Repartition" hint="Rewrites the partition table, requires a PIT and all packages. Risk of brick." onClick={() => setFlashOpts((o) => ({ ...o, repartition: !o.repartition }))} />
                      </div>
                    </details>
                  </Card>
                  {flashError && <div className="inline-error" role="alert"><WarningOctagon /> {flashError}</div>}
                </div>

                <div className={`flash-dock ${flashState === "error" ? "danger-card" : ""}`}>
                  {flashState === "idle" ? (
                    <div className="dock-idle">
                      <span>
                        <Usb />
                        {flashMode === "folder"
                          ? folderScanBusy ? "Scanning firmware folder" : selectedPackages.length ? `Folder scanned · ${selectedPackages.length} package${selectedPackages.length === 1 ? "" : "s"}` : folder ? "Scan the firmware folder" : "Select a firmware folder"
                          : `${selectedPackages.length} package${selectedPackages.length === 1 ? "" : "s"} ready`}
                        {" · "}
                        {deviceConnected ? "device in download mode" : "will wait for a device"}
                      </span>
                      <button className={destructive ? "primary danger" : "primary"} disabled={folderScanBusy || selectedPackages.length === 0} onClick={() => setFlashModal(true)}>
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

            {view === "device" && (
              <div className="stack wide">
                <div className="action-cards">
                  <ActionCard disabled={deviceBusy} icon={<MagnifyingGlass />} title="Detect download mode" text="Scan the selected USB backend" onClick={() => detectDevice(false)} />
                  <ActionCard disabled={deviceBusy} icon={<Table />} title="Read PIT" text="Inspect the device partition table" onClick={printPit} />
                  <ActionCard disabled={deviceBusy} icon={<ArrowUDownLeft />} title="Reboot to download" text="Send the supported reboot command" onClick={rebootDownload} />
                </div>
                {deviceError && <div className="inline-error" role="alert"><WarningOctagon /> {deviceError}</div>}
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
                {verifyError && <div className="inline-error" role="alert"><WarningOctagon /> {verifyError}</div>}
                {verifyRows.map((row) => (
                  <div className={`file-row ${row.status === "error" ? "file-error" : ""}`} key={row.file}>
                    {row.status === "ok" ? <SealCheck weight="fill" /> : row.status === "checking" ? <Circle className="spin" /> : row.status === "error" ? <WarningOctagon weight="fill" /> : <FileArchive />}
                    <code>{basename(row.file)}</code>
                    <span>{row.message}</span>
                    <button className="row-remove" disabled={row.status === "checking"} aria-label={`Remove ${basename(row.file)}`} onClick={() => setVerifyRows((rows) => rows.filter((item) => item.file !== row.file))}><X /></button>
                  </div>
                ))}
                {verifyRows.length > 0 && <div className="row"><button className="primary" disabled={verifyRows.some((row) => row.status === "checking")} onClick={verifyAll}><SealCheck weight="bold" /> Verify all</button><button className="ghost" disabled={verifyRows.some((row) => row.status === "checking")} onClick={() => { setVerifyRows([]); setVerifyError(""); }}>Clear list</button></div>}
              </div>
            )}

            {view === "settings" && (
              <div className="stack narrow">
                <Card>
                  <span className="label">Manual USB backend</span>
                  <p className="settings-help">Used only for download-mode detection and flashing. Keep the recommended default unless device detection fails.</p>
                  <div className="backend-list">
                    {["vcom", "nusb", "libusb"].map((item) => (
                      <button key={item} className={`backend-row ${backend === item ? "active" : ""}`} disabled={!appInfo.backends.includes(item)} onClick={() => setBackend(item)}>
                        <span><strong>{item}{item === appInfo.defaultBackend && <em className="recommended">Recommended</em>}</strong><small>{backendCopy(item, appInfo.backends.includes(item))}</small></span>
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

      {downloadPlan && (
        <div className="modal-backdrop">
          <div className="modal download-review" role="dialog" aria-modal="true" aria-labelledby="download-review-title">
            <div className="row">
              <div className="modal-icon"><DownloadSimple weight="fill" /></div>
              <div><h2 id="download-review-title">Review download</h2><p>Preflight completed. Confirm the destination before transferring.</p></div>
            </div>
            <div className="modal-list">
              <ModalItem label="Firmware" value={`${downloadPlan.model} · ${downloadPlan.region} · ${downloadPlan.version}`} />
              <ModalItem label="File" value={downloadPlan.filename} />
              <ModalItem label="Size" value={fmtBytes(downloadPlan.size)} />
              <ModalItem label="Free space" value={downloadPlan.availableSpace == null ? "Unavailable" : fmtBytes(downloadPlan.availableSpace)} />
              <ModalItem label="Destination" value={downloadPlan.path} />
            </div>
            {!downloadPlan.enoughSpace && <div className="inline-error" role="alert"><WarningOctagon /> Not enough free space. Choose a different output folder.</div>}
            {(downloadPlan.conflict || downloadPlan.partConflict) && (
              <div className="conflict-box">
                <div className="row"><WarningOctagon /><strong>{downloadPlan.conflict ? "A completed file already exists." : "An interrupted partial download exists."}</strong></div>
                {downloadPlan.partConflict && <p>Resume is not supported for this file. The partial file at <code>{downloadPlan.partPath}</code> will be replaced and the download will restart.</p>}
                <label className="replace-confirm"><input type="checkbox" checked={allowReplace} onChange={(event) => setAllowReplace(event.target.checked)} /> Replace the existing {downloadPlan.conflict && downloadPlan.partConflict ? "files" : "file"}</label>
              </div>
            )}
            <div className="modal-actions">
              <button ref={downloadCancelRef} className="ghost" onClick={() => { setDownloadPlan(null); setDownloadState("idle"); }}>Cancel</button>
              <button className="primary" disabled={!downloadPlan.enoughSpace || ((downloadPlan.conflict || downloadPlan.partConflict) && !allowReplace)} onClick={startDownload}>Start download</button>
            </div>
          </div>
        </div>
      )}

      {flashModal && (
        <div className="modal-backdrop">
          <div className="modal" role="dialog" aria-modal="true" aria-labelledby="flash-review-title">
            <div className="row">
              <div className={destructive ? "modal-icon danger" : "modal-icon"}>{destructive ? <WarningOctagon weight="fill" /> : <Lightning weight="fill" />}</div>
              <h2 id="flash-review-title">{destructive ? "Confirm destructive flash" : "Confirm flash"}</h2>
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
              <button ref={flashCancelRef} className="ghost" onClick={() => setFlashModal(false)}>Cancel</button>
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
  return <button className={`nav-button ${active ? "active" : ""}`} aria-label={label} title={label} aria-current={active ? "page" : undefined} onClick={onClick}>{icon}<span>{label}</span></button>;
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

function Progress({ value, ok, label = "Progress" }: { value: number; ok?: boolean; label?: string }) {
  const safeValue = Math.max(0, Math.min(value, 100));
  return <div className="progress" role="progressbar" aria-label={label} aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.round(safeValue)}><div className={ok ? "ok" : ""} style={{ width: `${safeValue}%` }} /></div>;
}

function Stat({ label, value, accent }: { label: string; value: string; accent?: boolean }) {
  return <div className="stat"><span>{label}</span><code className={accent ? "accent" : ""}>{value}</code></div>;
}

function CheckOption({ checked, title, hint, danger, onClick }: { checked: boolean; title: string; hint: string; danger?: boolean; onClick: () => void }) {
  return <button className={`check-option ${checked ? "checked" : ""} ${danger ? "danger-option" : ""}`} aria-pressed={checked} onClick={onClick}><span className="fake-check">{checked && <Check weight="bold" />}</span><span><strong>{title}{danger && <Warning weight="fill" />}</strong><small>{hint}</small></span></button>;
}

function PartitionRow({ name, part }: { name: string; part: FlashPart }) {
  return <div className="partition-row"><CheckCircle weight={part.pct >= 100 ? "fill" : "regular"} /><code>{name}</code><span>{Math.round(part.pct)}%</span><Progress value={part.pct} ok={part.pct >= 100} /><code>{fmtBytes(part.bytes)} / {fmtBytes(part.total)}</code></div>;
}

function ActionCard({ icon, title, text, onClick, disabled }: { icon: React.ReactNode; title: string; text: string; onClick: () => void; disabled?: boolean }) {
  return <button className="action-card" disabled={disabled} onClick={onClick}>{icon}<strong>{title}</strong><span>{text}</span></button>;
}

function Switch({ on, onClick }: { on: boolean; onClick: () => void }) {
  return <button className={`switch ${on ? "on" : ""}`} role="switch" aria-checked={on} onClick={onClick}><span /></button>;
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

function downloadStatusCopy(state: DownloadState) {
  if (state === "preparing") return "Checking availability and disk space";
  if (state === "downloading") return "Downloading and decrypting on the fly";
  if (state === "paused") return "Download paused";
  if (state === "canceling") return "Stopping safely...";
  if (state === "cancelled") return "Download cancelled";
  if (state === "done") return "Download complete";
  if (state === "error") return "Download failed";
  return "Ready";
}
