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
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open, save } from "@tauri-apps/plugin-dialog";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

type View = "firmware" | "flash" | "device" | "verify" | "settings";
type Theme = "dark" | "light";
type FlashMode = "pkg" | "folder";
type CscMode = "home" | "wipe";
type RunState = "idle" | "waiting" | "active" | "done" | "error";
type DownloadState = "idle" | "preparing" | "downloading" | "paused" | "canceling" | "cancelled" | "done" | "error";
type FirmwareFilter = "all" | "stable" | "beta";
type FirmwareKind = "latest" | "stable" | "beta";
type ActiveOperation = "download" | "flash-wait" | "flash" | "device";

export type FolderPackages = {
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

type FirmwareRetry = {
  row: FirmwareRow;
  model: string;
  region: string;
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

export type Preferences = {
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

type FlashRequestPayload = {
  backend: string;
  verbose: boolean;
  wait: boolean;
  noReboot: boolean;
  repartition: boolean;
  skipSizeCheck: boolean;
  skipMd5: boolean;
  pit: null;
  folder: string | null;
  cscMode: CscMode;
  packages: FolderPackages;
  reviewedPackages: PackageIdentity[] | null;
};

type PackageIdentity = {
  path: string;
  canonicalPath: string;
  size: number;
  modifiedUnixNanos: string;
};

type FolderScanResult = {
  packages: FolderPackages;
  identities: PackageIdentity[];
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

type DevicePitRead = {
  snapshotId: string;
  rows: PitRow[];
};

type VerifyRow = {
  file: string;
  status: "pending" | "checking" | "ok" | "error";
  message: string;
};

const basename = (path: string) => path.split(/[\\/]/).filter(Boolean).pop() ?? path;

// CLI examples target PowerShell on Windows. Single-quoted strings are literal;
// embedded apostrophes are represented by two apostrophes.
export const powerShellQuote = (value: string) => `'${value.replaceAll("'", "''")}'`;

export const downloadOutputCliFlag = (selectedDirectory: string, defaultDirectory = "") => {
  const directory = selectedDirectory.trim() || defaultDirectory.trim();
  return directory ? ` -d ${powerShellQuote(directory)}` : "";
};

export const deviceCliSubcommand = (hasPitRows: boolean) =>
  hasPitRows ? "print-pit --no-reboot" : "detect";

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

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value);

export const validatePreferences = (value: unknown): Partial<Preferences> => {
  if (!isRecord(value)) return {};
  const result: Partial<Preferences> = {};
  if (value.theme === "dark" || value.theme === "light") result.theme = value.theme;
  if (typeof value.backend === "string" && /^[a-z0-9_-]{1,24}$/i.test(value.backend)) result.backend = value.backend;
  if (typeof value.verbose === "boolean") result.verbose = value.verbose;
  if (typeof value.model === "string" && value.model.length <= 48) result.model = value.model;
  if (typeof value.region === "string" && value.region.length <= 3) result.region = value.region;
  if (typeof value.outDir === "string" && value.outDir.length <= 32_767) result.outDir = value.outDir;
  if (typeof value.threads === "number" && Number.isInteger(value.threads) && value.threads >= 1 && value.threads <= 16) result.threads = value.threads;
  return result;
};

export const readPreferences = (): Partial<Preferences> => {
  if (typeof window === "undefined") return {};
  try {
    return validatePreferences(JSON.parse(window.localStorage.getItem("samloader.preferences.v2") ?? "{}"));
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

const REQUIRED_REPARTITION_SLOTS = ["bl", "ap", "cp", "csc"] as const;

export const missingRepartitionSlots = (packages: FolderPackages) =>
  REQUIRED_REPARTITION_SLOTS.filter((slot) => !packages[slot]);

export const packagesForFlashRequest = (folderMode: boolean, packages: FolderPackages): FolderPackages =>
  folderMode ? {} : { ...packages };

export const isRegularCscPackage = (path?: string | null) => {
  if (!path) return false;
  const name = basename(path).toUpperCase();
  return /^CSC(?:_|[.-])/.test(name) && !/^HOME_CSC(?:_|[.-])/.test(name);
};

export const cscModeForFlashRequest = (
  folderMode: boolean,
  selectedCsc: string | null | undefined,
  folderCscMode: CscMode,
): CscMode => folderMode ? folderCscMode : isRegularCscPackage(selectedCsc) ? "wipe" : "home";

export const cscModeCliFlag = (
  folderMode: boolean,
  selectedCsc: string | null | undefined,
  folderCscMode: CscMode,
) => cscModeForFlashRequest(folderMode, selectedCsc, folderCscMode) === "wipe"
  ? " --csc-mode wipe"
  : "";

export const hasWipeCscForRepartition = (
  folderMode: boolean,
  packages: FolderPackages,
  folderCscMode: CscMode,
) => cscModeForFlashRequest(folderMode, packages.csc, folderCscMode) === "wipe";

export const canRepartition = (
  packages: FolderPackages,
  folderMode: boolean,
  folderCscMode: CscMode,
) => missingRepartitionSlots(packages).length === 0
  && hasWipeCscForRepartition(folderMode, packages, folderCscMode);

export const canStartDownloadPlan = (
  enoughSpace: boolean,
  completedFileConflict: boolean,
  partialFileConflict: boolean,
  allowReplace: boolean,
) => enoughSpace
  && !partialFileConflict
  && (!completedFileConflict || allowReplace);

export const firmwareIdentityMatches = (
  expectedModel: string,
  expectedRegion: string,
  model: string,
  region: string,
) => expectedModel.trim().toUpperCase() === model.trim().toUpperCase()
  && expectedRegion.trim().toUpperCase() === region.trim().toUpperCase();

export const detectedFirmwareIdentity = (device: Pick<AndroidDevice, "model" | "region">) => ({
  model: device.model?.trim().toUpperCase() ?? "",
  region: device.region?.trim().toUpperCase() ?? "",
});

export function App() {
  const saved = useRef(readPreferences()).current;
  const [appInfo, setAppInfo] = useState<AppInfo>({
    version: "2.0.2",
    defaultBackend: "vcom",
    backends: ["vcom", "nusb"],
  });
  const [theme, setTheme] = useState<Theme>(saved.theme === "light" ? "light" : "dark");
  const [view, setView] = useState<View>("firmware");
  const viewRef = useRef<View>("firmware");
  const [backend, setBackend] = useState(saved.backend ?? "vcom");
  const [verbose, setVerbose] = useState(saved.verbose ?? false);
  const [copied, setCopied] = useState(false);
  const [copyFailed, setCopyFailed] = useState(false);
  const [commandExpanded, setCommandExpanded] = useState(false);
  const [closeBlocked, setCloseBlocked] = useState<ActiveOperation | null>(null);
  const [closeBlockedMessage, setCloseBlockedMessage] = useState("");

  const [deviceConnected, setDeviceConnected] = useState(false);
  const [deviceBusy, setDeviceBusy] = useState(false);
  const [pitRows, setPitRows] = useState<PitRow[]>([]);
  const [pitSnapshotId, setPitSnapshotId] = useState<string | null>(null);
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
  const downloadPrepareGeneration = useRef(0);
  const [downloadControlBusy, setDownloadControlBusy] = useState(false);
  const [lastFirmware, setLastFirmware] = useState<FirmwareRetry | null>(null);

  const [firmwareState, setFirmwareState] = useState<RunState>("idle");
  const [firmwareError, setFirmwareError] = useState("");
  const [firmwareResults, setFirmwareResults] = useState<FirmwareResults | null>(null);
  const [firmwareFilter, setFirmwareFilter] = useState<FirmwareFilter>("all");
  const [firmwareQuery, setFirmwareQuery] = useState("");
  const [androidDevice, setAndroidDevice] = useState<AndroidDevice | null>(null);
  const [androidDetecting, setAndroidDetecting] = useState(false);
  const firmwareSearchGeneration = useRef(0);

  const [flashMode, setFlashMode] = useState<FlashMode>("folder");
  const [folder, setFolder] = useState("");
  const [folderScanBusy, setFolderScanBusy] = useState(false);
  const folderScanGeneration = useRef(0);
  const [reviewedFolderPackages, setReviewedFolderPackages] = useState<PackageIdentity[]>([]);
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
  const [flashAcknowledged, setFlashAcknowledged] = useState(false);
  const [flashConfirmation, setFlashConfirmation] = useState("");
  const flashWaitGeneration = useRef(0);

  const [verifyRows, setVerifyRows] = useState<VerifyRow[]>([]);
  const [verifyError, setVerifyError] = useState("");
  const downloadCancelRef = useRef<HTMLButtonElement>(null);
  const flashCancelRef = useRef<HTMLButtonElement>(null);
  const closeCancelRef = useRef<HTMLButtonElement>(null);
  const modalReturnFocus = useRef<HTMLElement | null>(null);
  const modalWasOpen = useRef(false);
  const activeOperationRef = useRef<ActiveOperation | null>(null);

  const refreshDeviceStatus = useCallback(async (wait = false, showBusy = false): Promise<DeviceStatus | null> => {
    if (deviceStatusInFlight.current) return null;
    deviceStatusInFlight.current = true;
    if (showBusy) setDeviceBusy(true);

    try {
      const result = await invoke<DeviceStatus>("detect_download_device", { backend, wait });
      setDeviceConnected(result.connected);
      setDeviceMessage(result.label);
      if (!result.connected) {
        setPitRows([]);
        setPitSnapshotId(null);
      }
      return result;
    } catch (error) {
      const message = errorMessage(error);
      setDeviceConnected(false);
      setPitRows([]);
      setPitSnapshotId(null);
      setDeviceMessage(showBusy ? "Download-mode detection failed" : "Unable to check download-mode device");
      if (showBusy) setDeviceError(message);
      return null;
    } finally {
      if (showBusy) setDeviceBusy(false);
      deviceStatusInFlight.current = false;
    }
  }, [backend]);

  const downloadLocked = ["preparing", "downloading", "paused", "canceling"].includes(downloadState);
  const downloadInFlight = ["downloading", "paused", "canceling"].includes(downloadState);
  const activeOperation: ActiveOperation | null = flashState === "active"
    ? "flash"
    : flashState === "waiting"
      ? "flash-wait"
      : downloadInFlight
        ? "download"
        : deviceBusy || androidDetecting
          ? "device"
          : null;
  const operationLocked = activeOperation !== null;
  const operationView: View | null = activeOperation === "download"
    ? "firmware"
    : activeOperation === "device"
      ? "device"
      : activeOperation
        ? "flash"
        : null;
  const modalOpen = Boolean(downloadPlan || flashModal || closeBlocked);
  activeOperationRef.current = activeOperation;
  viewRef.current = view;

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
      else if (payload.status === "error") {
        setFlashState("error");
        setFlashError(payload.message ?? "Flash failed.");
      }
      else setFlashState("active");
    });

    const unlistenCloseBlocked = listen<string>("close-blocked", (event) => {
      modalReturnFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
      setCloseBlockedMessage(event.payload || "An operation is still active. Keep samloader open until it finishes.");
      setCloseBlocked(activeOperationRef.current ?? "device");
    });

    return () => {
      void unlistenDownload.then((fn) => fn());
      void unlistenFlash.then((fn) => fn());
      void unlistenCloseBlocked.then((fn) => fn());
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
    setDeviceConnected(false);
    setPitRows([]);
    setPitSnapshotId(null);
    setDeviceError("");
    setDeviceMessage("Odin / download mode");
  }, [backend]);

  useEffect(() => {
    if (view === "firmware") return;
    downloadPrepareGeneration.current += 1;
    setPreparingVersion("");
    setDownloadState((current) => current === "preparing" ? "idle" : current);
  }, [view]);

  useEffect(() => {
    if (!hasTauriRuntime()) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void getCurrentWindow().onCloseRequested((event) => {
      const operation = activeOperationRef.current;
      if (!operation) return;
      event.preventDefault();
      modalReturnFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
      setCloseBlockedMessage("");
      setCloseBlocked(operation);
    }).then((dispose) => {
      if (disposed) dispose();
      else unlisten = dispose;
    }).catch(() => undefined);
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  useEffect(() => {
    if (!activeOperation) {
      setCloseBlocked(null);
      setCloseBlockedMessage("");
    }
  }, [activeOperation]);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        if (downloadPlan) setDownloadPlan(null);
        else if (flashModal) setFlashModal(false);
        else if (closeBlocked) setCloseBlocked(null);
        return;
      }
      if (event.key !== "Tab" || !modalOpen) return;

      const modal = document.querySelector<HTMLElement>(".modal-backdrop .modal");
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
  }, [closeBlocked, downloadPlan, flashModal, modalOpen]);

  useEffect(() => {
    if (modalOpen && !modalWasOpen.current && !modalReturnFocus.current) {
      modalReturnFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    } else if (!modalOpen && modalWasOpen.current) {
      const returnTarget = modalReturnFocus.current;
      window.setTimeout(() => returnTarget?.focus(), 0);
      modalReturnFocus.current = null;
    }
    modalWasOpen.current = modalOpen;
  }, [modalOpen]);

  useEffect(() => {
    if (downloadPlan) window.setTimeout(() => downloadCancelRef.current?.focus(), 0);
  }, [downloadPlan]);

  useEffect(() => {
    if (flashModal) window.setTimeout(() => flashCancelRef.current?.focus(), 0);
  }, [flashModal]);

  useEffect(() => {
    if (closeBlocked) window.setTimeout(() => closeCancelRef.current?.focus(), 0);
  }, [closeBlocked]);

  useEffect(() => () => {
    flashWaitGeneration.current += 1;
    firmwareSearchGeneration.current += 1;
  }, []);

  useEffect(() => {
    if (!hasTauriRuntime() || operationLocked || modalOpen) return;

    void refreshDeviceStatus(false, false);
    const interval = window.setInterval(() => {
      void refreshDeviceStatus(false, false);
    }, 2500);

    return () => window.clearInterval(interval);
  }, [modalOpen, operationLocked, refreshDeviceStatus]);

  const selectedPackages = packageList(packages);
  const missingCorePackages = missingRepartitionSlots(packages);
  const unverifiedTarPackages = selectedPackages.filter((path) => path.toLowerCase().endsWith(".tar") && !path.toLowerCase().endsWith(".tar.md5"));
  const manualCscFactoryReset = flashMode === "pkg" && isRegularCscPackage(packages.csc);
  const factoryReset = (flashMode === "folder" && cscMode === "wipe") || manualCscFactoryReset;
  const repartitionHasWipeCsc = hasWipeCscForRepartition(flashMode === "folder", packages, cscMode);
  const destructive = flashOpts.repartition || factoryReset;
  const folderReviewValid = flashMode !== "folder" || reviewedFolderPackages.length === selectedPackages.length;
  const flashPhrase = flashOpts.repartition ? "REPARTITION" : factoryReset ? "ERASE" : "";
  const flashConfirmationValid = flashAcknowledged && (!flashPhrase || flashConfirmation.trim().toUpperCase() === flashPhrase);
  const flashReady = selectedPackages.length > 0
    && !folderScanBusy
    && folderReviewValid
    && (!flashOpts.repartition || canRepartition(packages, flashMode === "folder", cscMode))
    && (deviceConnected || flashOpts.wait);
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
  const connectionThreads = manualConnections ? dThreads : 8;
  const command = useMemo(() => {
    const globals = `${backend ? ` --usb-backend ${powerShellQuote(backend)}` : ""}${verbose ? " --verbose" : ""}`;
    if (view === "firmware") {
      if (!dVersion) return `samloader${globals} check-update -m ${powerShellQuote(dModel || "MODEL")} -r ${powerShellQuote(dRegion || "CSC")} --all`;
      return `samloader${globals} download -m ${powerShellQuote(dModel || "MODEL")} -r ${powerShellQuote(dRegion || "CSC")} -v ${powerShellQuote(dVersion)} -j ${connectionThreads}${downloadOutputCliFlag(dOut, appInfo.defaultDownloadDir)}`;
    }
    if (view === "flash") {
      const opts = `${flashOpts.wait ? " --wait" : ""}${flashOpts.noReboot ? " --no-reboot" : ""}${flashOpts.skipMd5 ? " --skip-md5" : ""}${flashOpts.skipSize ? " --skip-size-check" : ""}${flashOpts.repartition ? " --repartition" : ""}`;
      if (flashMode === "folder") {
        return `samloader${globals} flash${opts} --folder ${powerShellQuote(folder || "<folder>")}${cscModeCliFlag(true, packages.csc, cscMode)}`;
      }
      return `samloader${globals} flash${opts}${cscModeCliFlag(false, packages.csc, cscMode)}${packages.bl ? ` -b ${powerShellQuote(packages.bl)}` : ""}${packages.ap ? ` -a ${powerShellQuote(packages.ap)}` : ""}${packages.cp ? ` -c ${powerShellQuote(packages.cp)}` : ""}${packages.csc ? ` -s ${powerShellQuote(packages.csc)}` : ""}${packages.userdata ? ` -u ${powerShellQuote(packages.userdata)}` : ""}`;
    }
    if (view === "device") return `samloader${globals} ${deviceCliSubcommand(pitRows.length > 0)}`;
    if (view === "verify") return `samloader verify-md5 ${verifyRows.map((row) => powerShellQuote(row.file)).join(" ") || powerShellQuote("<files>")}`;
    return `samloader${globals} --help`;
  }, [appInfo.defaultDownloadDir, backend, connectionThreads, cscMode, dModel, dOut, dRegion, dVersion, flashMode, flashOpts, folder, packages, pitRows.length, verbose, verifyRows, view]);

  const title = {
    firmware: ["Firmware Explorer", "Find every known stable and beta build, then download it safely."],
    flash: ["Flash firmware", "Write packages or an extracted folder to your device."],
    device: ["Device & PIT", "Detect the device, inspect PIT, or reboot to download mode."],
    verify: ["Verify MD5", "Check downloaded .tar.md5 package integrity."],
    settings: ["Settings", "USB backend, logging, and appearance."],
  }[view];

  async function copyCommand() {
    const showCopied = () => {
      setCopyFailed(false);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1400);
    };

    try {
      await navigator.clipboard.writeText(command);
      showCopied();
      return;
    } catch {
      // WebView clipboard access can be unavailable under restrictive policies.
    }

    const textarea = document.createElement("textarea");
    textarea.value = command;
    textarea.setAttribute("readonly", "");
    textarea.style.position = "fixed";
    textarea.style.opacity = "0";
    document.body.appendChild(textarea);
    textarea.select();
    const copiedWithFallback = document.execCommand("copy");
    textarea.remove();
    if (copiedWithFallback) {
      showCopied();
    } else {
      setCopyFailed(true);
      window.setTimeout(() => setCopyFailed(false), 2400);
    }
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
    setReviewedFolderPackages([]);
    try {
      const result = await invoke<FolderScanResult>("scan_firmware_folder", { folder: path, cscMode: mode });
      if (generation !== folderScanGeneration.current) return;
      setPackages(result.packages);
      setReviewedFolderPackages(result.identities);
      if (packageList(result.packages).length === 0) setFlashError("No BL, AP, CP, CSC, or USERDATA packages were found in this folder.");
    } catch (error) {
      if (generation !== folderScanGeneration.current) return;
      setFlashError(errorMessage(error));
    } finally {
      if (generation === folderScanGeneration.current) setFolderScanBusy(false);
    }
  }

  function editFirmwareIdentity(field: "model" | "region", value: string) {
    firmwareSearchGeneration.current += 1;
    if (field === "model") setDModel(value.toUpperCase());
    else setDRegion(value.toUpperCase());
    setDVersion("");
    setFirmwareResults(null);
    setFirmwareState("idle");
    setFirmwareError("");
    setFirmwareFilter("all");
    setFirmwareQuery("");
    setAndroidDevice(null);
    setLastFirmware(null);
    setDownloadState("idle");
    setDownloadEvent(null);
    setDownloadError("");
  }

  function validateFirmwareIdentity(model = dModel, region = dRegion) {
    const cleanModel = model.trim().toUpperCase();
    const cleanRegion = region.trim().toUpperCase();
    if (!/^[A-Z0-9_.-]{2,48}$/.test(cleanModel)) return "Enter a valid 2–48 character device model, for example SM-S931U1.";
    if (!/^[A-Z0-9]{3}$/.test(cleanRegion)) return "Enter a three-character sales code (CSC), for example XAA.";
    return "";
  }

  async function searchFirmwares(model = dModel, region = dRegion) {
    const generation = ++firmwareSearchGeneration.current;
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
      if (generation !== firmwareSearchGeneration.current) return;
      setFirmwareResults({
        latest: result.latest ?? "",
        previous: Array.isArray(result.previous) ? result.previous : [],
        beta: Array.isArray(result.beta) ? result.beta : [],
      });
      setFirmwareState("done");
    } catch (error) {
      if (generation !== firmwareSearchGeneration.current) return;
      setFirmwareState("error");
      setFirmwareError(errorMessage(error));
    }
  }

  async function useConnectedDevice() {
    if (androidDetecting) return;
    const generation = ++firmwareSearchGeneration.current;
    setAndroidDetecting(true);
    setFirmwareError("");
    setFirmwareResults(null);
    setDVersion("");
    setLastFirmware(null);
    setDownloadState("idle");
    setDownloadEvent(null);
    setDownloadError("");
    try {
      const result = await invoke<AndroidDevice>("detect_android_device");
      if (generation !== firmwareSearchGeneration.current) return;
      setAndroidDevice(result);
      if (!result.connected) {
        setFirmwareError(result.message || "No authorized Android device was found. Connect it with USB debugging enabled.");
        return;
      }
      const { model, region } = detectedFirmwareIdentity(result);
      // Assign both values so an incomplete second device cannot inherit the
      // previous device's model or sales CSC.
      setDModel(model);
      setDRegion(region);
      if (!model || !region) {
        setFirmwareError(`${result.message || "Device detected."} ${!model && !region ? "Model and CSC could not be read." : !model ? "Model could not be read." : "CSC could not be read."} Enter the missing value to search.`);
        return;
      }
      await searchFirmwares(model, region);
    } catch (error) {
      if (generation !== firmwareSearchGeneration.current) return;
      setFirmwareError(errorMessage(error));
    } finally {
      setAndroidDetecting(false);
    }
  }

  async function prepareDownload(row: FirmwareRow) {
    if (downloadLocked || preparingVersion) return;
    const generation = ++downloadPrepareGeneration.current;
    const validation = validateFirmwareIdentity();
    if (validation) {
      setFirmwareError(validation);
      return;
    }
    setDVersion(row.version);
    setLastFirmware({
      row,
      model: dModel.trim().toUpperCase(),
      region: dRegion.trim().toUpperCase(),
    });
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
      if (generation !== downloadPrepareGeneration.current || viewRef.current !== "firmware") return;
      modalReturnFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
      setDownloadPlan(plan);
      setDownloadState("idle");
    } catch (error) {
      if (generation !== downloadPrepareGeneration.current || viewRef.current !== "firmware") return;
      setDownloadState("error");
      setDownloadError(`${row.kind === "beta" ? "This beta may no longer be available from Samsung. " : ""}${errorMessage(error)}`);
    } finally {
      if (generation === downloadPrepareGeneration.current) setPreparingVersion("");
    }
  }

  async function startDownload() {
    if (!downloadPlan || downloadLocked) return;
    const plan = downloadPlan;
    if (!canStartDownloadPlan(plan.enoughSpace, plan.conflict, plan.partConflict, allowReplace)) return;
    setDownloadPlan(null);
    setDownloadError("");
    setDownloadState("downloading");
    setDownloadEvent({ status: "started", filename: plan.filename, path: plan.path, bytes: 0, total: plan.size, speedBps: 0, message: "Starting secure download" });
    try {
      await invoke("start_download", {
        req: {
          model: plan.model, region: plan.region, version: plan.version,
          outDir: dOut.trim() || null, outFile: null, threads: connectionThreads, verbose,
          overwrite: plan.conflict && allowReplace,
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
    setPitRows([]);
    setPitSnapshotId(null);
    setDeviceError("");
    await refreshDeviceStatus(wait, true);
  }

  async function printPit() {
    setPitRows([]);
    setPitSnapshotId(null);
    setDeviceError("");
    if (!deviceConnected) {
      setDeviceError("No Download Mode device is connected. Connect the device, then run Detect download mode before reading its PIT.");
      return;
    }
    setDeviceBusy(true);
    try {
      const result = await invoke<DevicePitRead>("read_device_pit", { backend, wait: false, verbose });
      setPitRows(result.rows);
      setPitSnapshotId(result.snapshotId);
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
    if (!deviceConnected || pitRows.length === 0 || !pitSnapshotId) {
      setPitRows([]);
      setPitSnapshotId(null);
      setDeviceError("The PIT view is stale or no device is connected. Detect the Download Mode device and read its PIT again.");
      return;
    }
    try {
      const output = await save({ filters: [{ name: "PIT", extensions: ["pit"] }] });
      if (!output) return;
      setDeviceBusy(true);
      await invoke("dump_device_pit", { snapshotId: pitSnapshotId, output });
    } catch (error) {
      setPitRows([]);
      setPitSnapshotId(null);
      setDeviceConnected(false);
      setDeviceError(errorMessage(error));
    } finally {
      setDeviceBusy(false);
    }
  }

  async function rebootDownload() {
    setDeviceBusy(true);
    setDeviceError("");
    setPitRows([]);
    setPitSnapshotId(null);
    try {
      await invoke("reboot_to_download", { backend });
      setDeviceConnected(false);
      setDeviceMessage("Reboot command sent · detect the device after it reconnects");
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

  async function startFlashRequest(req: FlashRequestPayload) {
    setFlashState("active");
    setFlashParts({});
    setFlashOverall({ bytes: 0, total: 0, pct: 0 });
    setFlashMessage("Preparing flash");
    setFlashError("");
    try {
      await invoke("start_flash", { req: { ...req, wait: false } });
    } catch (error) {
      const message = errorMessage(error);
      setFlashState("error");
      setFlashError(message);
      setFlashMessage(message);
    }
  }

  async function waitForFlashDevice(req: FlashRequestPayload) {
    const generation = ++flashWaitGeneration.current;
    setFlashState("waiting");
    setFlashParts({});
    setFlashOverall({ bytes: 0, total: 0, pct: 0 });
    setFlashError("");
    setFlashMessage("Waiting for a Download Mode device · connect it when ready");

    while (generation === flashWaitGeneration.current) {
      try {
        const status = await invoke<DeviceStatus>("detect_download_device", { backend: req.backend, wait: false });
        if (generation !== flashWaitGeneration.current) return;
        setDeviceConnected(status.connected);
        setDeviceMessage(status.label);
        if (status.connected) {
          setFlashMessage("Device detected · starting flash");
          await startFlashRequest(req);
          return;
        }
      } catch (error) {
        if (generation !== flashWaitGeneration.current) return;
        setDeviceConnected(false);
        setPitRows([]);
        setPitSnapshotId(null);
        setFlashMessage(`Still waiting · ${errorMessage(error)}`);
      }
      await new Promise<void>((resolve) => window.setTimeout(resolve, 1200));
    }
  }

  function cancelFlashWait() {
    flashWaitGeneration.current += 1;
    setFlashState("idle");
    setFlashMessage("Device wait cancelled safely");
    setFlashError("");
    setCloseBlocked(null);
  }

  async function confirmFlash() {
    if (!flashReady || !flashConfirmationValid) return;
    const req: FlashRequestPayload = {
      backend,
      verbose,
      wait: flashOpts.wait,
      noReboot: flashOpts.noReboot,
      repartition: flashOpts.repartition,
      skipSizeCheck: flashOpts.skipSize,
      skipMd5: flashOpts.skipMd5,
      pit: null,
      folder: flashMode === "folder" ? folder : null,
      cscMode: cscModeForFlashRequest(flashMode === "folder", packages.csc, cscMode),
      // The backend re-scans folder mode immediately before flashing so the
      // preview cannot become a second, stale package source of truth.
      packages: packagesForFlashRequest(flashMode === "folder", packages),
      reviewedPackages: flashMode === "folder" ? reviewedFolderPackages : null,
    };
    setFlashModal(false);
    setFlashAcknowledged(false);
    setFlashConfirmation("");
    if (deviceConnected && !flashOpts.wait) await startFlashRequest(req);
    else void waitForFlashDevice(req);
  }

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
  const operationMessage = activeOperation === "download"
    ? downloadState === "paused" ? "Firmware download paused · keep samloader open" : downloadState === "canceling" ? "Stopping download safely · keep samloader open" : "Firmware download in progress · keep samloader open"
    : activeOperation === "flash-wait"
      ? "Waiting for a Download Mode device · you can cancel safely"
      : activeOperation === "flash"
        ? "Device flash in progress · do not disconnect or close samloader"
        : activeOperation === "device"
          ? "Device operation in progress · keep samloader open"
          : "";
  const completionAnnouncement = downloadState === "done"
    ? "Firmware download complete."
    : downloadState === "cancelled"
      ? "Firmware download cancelled."
      : downloadState === "error"
        ? `Firmware download failed. ${downloadError}`
        : flashState === "done"
          ? "Device flash complete."
          : flashState === "error"
            ? `Device flash failed. ${flashError}`
            : "";

  return (
    <div className="app" data-theme={theme}>
      <div className="app-surface" aria-hidden={modalOpen ? true : undefined} inert={modalOpen ? true : undefined}>
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
            <NavButton disabled={operationLocked && operationView !== "firmware"} icon={<MagnifyingGlass />} label="Firmware Explorer" active={view === "firmware"} onClick={() => setView("firmware")} />
            <NavButton disabled={operationLocked && operationView !== "flash"} icon={<Lightning />} label="Flash" active={view === "flash"} onClick={() => setView("flash")} />
          </NavGroup>
          <NavGroup label="Device tools">
            <NavButton disabled={operationLocked} icon={<DeviceMobile />} label="Device & PIT" active={view === "device"} onClick={() => setView("device")} />
            <NavButton disabled={operationLocked} icon={<SealCheck />} label="Verify MD5" active={view === "verify"} onClick={() => setView("verify")} />
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
              disabled={operationLocked}
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

          {operationLocked && (
            <div className={`operation-banner ${activeOperation === "flash" ? "danger" : ""}`} role="status" aria-live="polite">
              <span><StatusDot connected={false} busy />{operationMessage}</span>
              {operationView && view !== operationView && <button className="ghost" onClick={() => setView(operationView)}>View operation</button>}
            </div>
          )}
          <div className="sr-only" role="status" aria-live="assertive" aria-atomic="true">{completionAnnouncement}</div>

          <section className={`content ${view === "flash" ? "flash-content" : ""}`}>
            {view === "firmware" && (
              <div className="stack wide firmware-stack">
                <Card className="firmware-search-card">
                  <div className="firmware-search-grid">
                    <Field label="Device model">
                      <input disabled={operationLocked || Boolean(preparingVersion)} value={dModel} onChange={(e) => editFirmwareIdentity("model", e.target.value)} placeholder="SM-S931U1" autoComplete="off" aria-describedby="model-help" />
                    </Field>
                    <Field label="Sales code (CSC)">
                      <input disabled={operationLocked || Boolean(preparingVersion)} value={dRegion} onChange={(e) => editFirmwareIdentity("region", e.target.value)} placeholder="XAA" list="known-csc-codes" autoComplete="off" aria-describedby="csc-help" />
                      <datalist id="known-csc-codes">{KNOWN_CSC_CODES.map(([code, label]) => <option key={code} value={code}>{label}</option>)}</datalist>
                    </Field>
                    <button className="ghost detect-android" disabled={androidDetecting || firmwareState === "active" || operationLocked || Boolean(preparingVersion)} onClick={useConnectedDevice}>
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
                        <input disabled={operationLocked || Boolean(preparingVersion)} value={dOut} onChange={(e) => setDOut(e.target.value)} placeholder={appInfo.defaultDownloadDir || "System Downloads folder"} />
                        <button disabled={operationLocked || Boolean(preparingVersion)} aria-label="Choose output folder" onClick={() => pickDirectory(setDOut)}><FolderOpen /></button>
                      </div>
                    </Field>
                    <div className="threads-control" aria-label="Connection strategy">
                      <div className="row between"><span className="label">Connections</span><code className="accent">{manualConnections ? dThreads : "Auto · 8"}</code></div>
                      <div className="connection-choice">
                        <button disabled={operationLocked || Boolean(preparingVersion)} className={!manualConnections ? "active" : ""} aria-pressed={!manualConnections} onClick={() => setManualConnections(false)}>Auto</button>
                        <button disabled={operationLocked || Boolean(preparingVersion)} className={manualConnections ? "active" : ""} aria-pressed={manualConnections} onClick={() => setManualConnections(true)}>Manual</button>
                      </div>
                      {manualConnections ? <input disabled={operationLocked || Boolean(preparingVersion)} aria-label="Parallel download connections" className="range" type="range" min={1} max={16} value={dThreads} onChange={(e) => setDThreads(Number(e.target.value))} /> : <small>Starts at 8 and reduces if the server throttles.</small>}
                    </div>
                    <button className="primary search-button" disabled={firmwareState === "active" || Boolean(preparingVersion) || operationLocked} onClick={() => searchFirmwares()}>
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
                    <div className="firmware-table-wrap">
                      <table className="firmware-table" aria-label="Available firmware versions">
                        <thead><tr className="firmware-table-head"><th scope="col">Version</th><th scope="col">Channel</th><th scope="col">Availability</th><th scope="col"><span className="sr-only">Action</span></th></tr></thead>
                        <tbody>{visibleFirmwareRows.map((row) => (
                          <tr className="firmware-row" key={`${row.kind}-${row.version}`}>
                            <td><code title={row.version}>{row.version}</code></td>
                            <td><span className={`firmware-badge ${row.kind}`}>{row.kind === "latest" ? "Latest stable" : row.kind}</span></td>
                            <td className="availability">Checked during preflight</td>
                            <td><button className="ghost" disabled={operationLocked || Boolean(preparingVersion)} onClick={() => prepareDownload(row)}>
                              {preparingVersion === row.version ? <ArrowsClockwise className="spin" /> : <DownloadSimple />} {preparingVersion === row.version ? "Checking..." : "Download"}
                            </button></td>
                          </tr>
                        ))}</tbody>
                      </table>
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
                      {["error", "cancelled"].includes(downloadState) && lastFirmware && firmwareIdentityMatches(lastFirmware.model, lastFirmware.region, dModel, dRegion) && <button className="primary compact-button" onClick={() => prepareDownload(lastFirmware.row)}><ArrowsClockwise /> Retry</button>}
                      {["done", "error", "cancelled"].includes(downloadState) && <button className="ghost" onClick={() => { setDownloadState("idle"); setDownloadEvent(null); setDownloadError(""); setDVersion(""); }}>Clear</button>}
                    </div>
                  </Card>
                )}
              </div>
            )}

            {view === "flash" && (
              <div className="stack wide flash-stack">
                <fieldset className="flash-config operation-fieldset" disabled={operationLocked}>
                  <div className="segments">
                    <button className={flashMode === "pkg" ? "active" : ""} onClick={() => { folderScanGeneration.current += 1; setFolderScanBusy(false); setFlashMode("pkg"); setPackages({}); setReviewedFolderPackages([]); setFlashError(""); }}>Package files</button>
                    <button className={flashMode === "folder" ? "active" : ""} onClick={() => { folderScanGeneration.current += 1; setFolderScanBusy(false); setFlashMode("folder"); setPackages({}); setReviewedFolderPackages([]); setFlashError(""); }}>Firmware folder</button>
                  </div>

                  {flashMode === "folder" ? (
                    <Card className="flash-folder-card">
                      <Field label="Extracted firmware folder">
                        <div className="input-action">
                          <input value={folder} onChange={(e) => { folderScanGeneration.current += 1; setFolderScanBusy(false); setFolder(e.target.value); setPackages({}); setReviewedFolderPackages([]); setFlashError(""); }} placeholder="D:\\SAMFW..." />
                          <button aria-label="Scan typed firmware folder" disabled={!folder.trim() || folderScanBusy} onClick={() => scanFolder()}><MagnifyingGlass className={folderScanBusy ? "spin" : ""} /></button>
                          <button aria-label="Choose firmware folder" disabled={folderScanBusy} onClick={async () => {
                            setFlashError("");
                            try {
                              const selected = await open({ directory: true, multiple: false });
                              if (typeof selected === "string") {
                                setFolder(selected);
                                setPackages({});
                                setReviewedFolderPackages([]);
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
                      {manualCscFactoryReset && <div className="warning-row"><WarningOctagon /> The selected regular CSC package will factory reset the device. Use HOME_CSC to preserve data.</div>}
                    </Card>
                  )}

                  {unverifiedTarPackages.length > 0 && (
                    <div className="warning-row" role="status"><Warning /> {unverifiedTarPackages.length} selected {unverifiedTarPackages.length === 1 ? "package is" : "packages are"} plain .tar with no MD5 footer and cannot be integrity-verified.</div>
                  )}

                  <Card className="flash-options-card">
                    <span className="label">Flash behavior</span>
                    <div className="option-grid">
                      <CheckOption checked={flashOpts.noReboot} title="No auto-reboot" hint="Stay in download mode after flashing" onClick={() => setFlashOpts((o) => ({ ...o, noReboot: !o.noReboot }))} />
                      <CheckOption checked={flashOpts.wait} title="Wait for device" hint="Poll in the app until a device connects; waiting can be cancelled" onClick={() => setFlashOpts((o) => ({ ...o, wait: !o.wait }))} />
                    </div>
                    <details className="advanced-options" open={flashOpts.skipMd5 || flashOpts.skipSize || flashOpts.repartition || undefined}>
                      <summary>Advanced and risky options</summary>
                      <p>Keep these checks enabled unless you understand the failure mode.</p>
                      <div className="option-grid">
                        <CheckOption checked={flashOpts.skipMd5} title="Skip MD5 check" hint="Disables package integrity protection" onClick={() => setFlashOpts((o) => ({ ...o, skipMd5: !o.skipMd5 }))} />
                        <CheckOption checked={flashOpts.skipSize} title="Skip size check" hint="Disables partition fit protection" onClick={() => setFlashOpts((o) => ({ ...o, skipSize: !o.skipSize }))} />
                        <CheckOption danger checked={flashOpts.repartition} title="Repartition" hint="Rewrites the PIT and requires BL, AP, CP, and regular CSC (factory reset). USERDATA is optional. Risk of brick." onClick={() => setFlashOpts((o) => ({ ...o, repartition: !o.repartition }))} />
                      </div>
                    </details>
                  </Card>
                  {flashOpts.repartition && missingCorePackages.length > 0 && <div className="inline-error" role="alert"><WarningOctagon /> Repartition is locked until these core slots are selected: {missingCorePackages.map((slot) => slot.toUpperCase()).join(", ")}.</div>}
                  {flashOpts.repartition && missingCorePackages.length === 0 && !repartitionHasWipeCsc && <div className="inline-error" role="alert"><WarningOctagon /> Repartition requires regular CSC and a factory reset. HOME_CSC cannot be used; select CSC · factory reset.</div>}
                  {flashError && <div className="inline-error" role="alert"><WarningOctagon /> {flashError}</div>}
                </fieldset>

                <div className={`flash-dock ${flashState === "error" ? "danger-card" : ""}`}>
                  {flashState === "idle" ? (
                    <div className="dock-idle">
                      <span>
                        <Usb />
                        {flashMode === "folder"
                          ? folderScanBusy ? "Scanning firmware folder" : selectedPackages.length ? `Folder scanned · ${selectedPackages.length} package${selectedPackages.length === 1 ? "" : "s"}` : folder ? "Scan the firmware folder" : "Select a firmware folder"
                          : `${selectedPackages.length} package${selectedPackages.length === 1 ? "" : "s"} ready`}
                        {" · "}
                        {deviceConnected ? "device in download mode" : flashOpts.wait ? "waiting is enabled" : "connect a device or enable Wait for device"}
                      </span>
                      <button className={destructive ? "primary danger" : "primary"} disabled={!flashReady} title={!flashReady ? flashReadinessMessage(selectedPackages.length, folderScanBusy, deviceConnected, flashOpts.wait, flashOpts.repartition, missingCorePackages, repartitionHasWipeCsc, folderReviewValid) : undefined} onClick={() => { modalReturnFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null; setFlashAcknowledged(false); setFlashConfirmation(""); setFlashModal(true); }}>
                        <Lightning weight="bold" /> Flash device
                      </button>
                    </div>
                  ) : (
                    <div className="dock-live">
                      <div className="dock-head">
                        <div className="row"><Usb className={["waiting", "active"].includes(flashState) ? "blink" : ""} /><strong>{flashState === "waiting" ? "Waiting for device" : flashState === "done" ? "Flash complete" : flashState === "error" ? "Flash failed" : "Flashing partitions"}</strong></div>
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
                      {flashState === "waiting" ? (
                        <div className="dock-actions"><button className="ghost danger-outline" onClick={cancelFlashWait}><X /> Cancel waiting</button></div>
                      ) : flashState !== "active" && (
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
                  <ActionCard disabled={deviceBusy || operationLocked} icon={<MagnifyingGlass />} title="Detect download mode" text="Scan once using the selected USB backend" onClick={() => detectDevice(false)} />
                  <ActionCard disabled={deviceBusy || operationLocked} icon={<Table />} title="Read PIT" text="Requires a detected Download Mode device" onClick={printPit} />
                  <ActionCard disabled={deviceBusy || operationLocked} icon={<ArrowUDownLeft />} title="Reboot to download" text="Send the supported reboot command" onClick={rebootDownload} />
                </div>
                {deviceError && <div className="inline-error" role="alert"><WarningOctagon /> {deviceError}</div>}
                {pitRows.length > 0 && (
                  <Card rise>
                    <div className="row between"><strong>PIT entries</strong><button className="ghost" disabled={operationLocked} onClick={dumpPit}><DownloadSimple /> Dump to file</button></div>
                    <div className="pit-table-wrap">
                      <table className="pit-table" aria-label="Device partition table">
                        <thead><tr className="pit-head"><th scope="col">ID</th><th scope="col">Partition</th><th scope="col">Flash file</th><th scope="col">Size</th></tr></thead>
                        <tbody>{pitRows.map((row) => (
                          <tr className="pit-row" key={`${row.id}-${row.partition}`}>
                            <td><code>{row.id}</code></td><td><strong>{row.partition}</strong></td><td><code>{row.flashFile}</code></td><td><code>{fmtBytes(row.size)}</code></td>
                          </tr>
                        ))}</tbody>
                      </table>
                    </div>
                  </Card>
                )}
              </div>
            )}

            {view === "verify" && (
              <div className="stack narrow">
                <button className="dropzone" disabled={operationLocked} onClick={addVerifyFiles}><FilePlus /><strong>Add .tar.md5 files</strong><span>MD5 footer verification</span></button>
                {verifyError && <div className="inline-error" role="alert"><WarningOctagon /> {verifyError}</div>}
                {verifyRows.map((row) => (
                  <div className={`file-row ${row.status === "error" ? "file-error" : ""}`} key={row.file}>
                    {row.status === "ok" ? <SealCheck weight="fill" /> : row.status === "checking" ? <Circle className="spin" /> : row.status === "error" ? <WarningOctagon weight="fill" /> : <FileArchive />}
                    <code>{basename(row.file)}</code>
                    <span>{row.message}</span>
                    <button className="row-remove" disabled={row.status === "checking"} aria-label={`Remove ${basename(row.file)}`} onClick={() => setVerifyRows((rows) => rows.filter((item) => item.file !== row.file))}><X /></button>
                  </div>
                ))}
                {verifyRows.length > 0 && <div className="row"><button className="primary" disabled={operationLocked || verifyRows.some((row) => row.status === "checking")} onClick={verifyAll}><SealCheck weight="bold" /> Verify all</button><button className="ghost" disabled={operationLocked || verifyRows.some((row) => row.status === "checking")} onClick={() => { setVerifyRows([]); setVerifyError(""); }}>Clear list</button></div>}
              </div>
            )}

            {view === "settings" && (
              <div className="stack narrow">
                <Card>
                  <span className="label">Manual USB backend</span>
                  <p className="settings-help">Used only for download-mode detection and flashing. Keep the recommended default unless device detection fails.</p>
                  <div className="backend-list">
                    {["vcom", "nusb", "libusb"].map((item) => (
                      <button key={item} className={`backend-row ${backend === item ? "active" : ""}`} disabled={operationLocked || !appInfo.backends.includes(item)} onClick={() => setBackend(item)}>
                        <span><strong>{item}{item === appInfo.defaultBackend && <em className="recommended">Recommended</em>}</strong><small>{backendCopy(item, appInfo.backends.includes(item))}</small></span>
                        {backend === item && <CheckCircle weight="fill" />}
                      </button>
                    ))}
                  </div>
                </Card>
                <Card>
                  <div className="setting-row"><span><strong>Verbose output</strong><small>Show extra download details. For full USB diagnostics, run the visible CLI command in PowerShell.</small></span><Switch label="Verbose output" disabled={operationLocked} on={verbose} onClick={() => setVerbose(!verbose)} /></div>
                  <div className="setting-row"><span><strong>Appearance</strong><small>{theme === "dark" ? "Dark" : "Light"} theme</small></span><button className="ghost" onClick={() => setTheme(theme === "dark" ? "light" : "dark")}>{theme === "dark" ? <Sun /> : <Moon />} Toggle</button></div>
                  <div className="setting-row"><span><strong>Diagnostic logs</strong><small>Bounded desktop logs are retained at <code>%LOCALAPPDATA%\com.samloader.desktop\logs</code>.</small></span></div>
                </Card>
                <Card className="about-card">
                  <div className="row between"><div><span className="label">About & licenses</span><h2>samloader <code>v{appInfo.version || "2.0.2"}</code></h2></div><SealCheck weight="fill" /></div>
                  <p>A consumer desktop interface for Samsung firmware discovery, download, verification, and flashing.</p>
                  <div className="license-list">
                    <div><strong>samloader</strong><span>Apache License 2.0</span></div>
                    <div><strong>Instrument Sans</strong><span>SIL Open Font License 1.1</span></div>
                    <div><strong>JetBrains Mono</strong><span>SIL Open Font License 1.1</span></div>
                  </div>
                  <p className="settings-help">Project license texts, third-party notices, and a generated dependency license inventory are embedded in this build.</p>
                  <div className="license-actions">
                    <a className="ghost" href="./licenses/THIRD_PARTY_NOTICES.md" download>Third-party notices</a>
                    <a className="ghost" href="./licenses/DEPENDENCY_LICENSES.md" download>Dependency inventory</a>
                  </div>
                </Card>
              </div>
            )}
          </section>

          <footer className={`command-bar ${commandExpanded ? "expanded" : ""}`}>
            <TerminalWindow />
            <code title={command} tabIndex={0} aria-label="Equivalent PowerShell CLI command">{command}</code>
            <button className="command-expand" aria-expanded={commandExpanded} onClick={() => setCommandExpanded((expanded) => !expanded)}>{commandExpanded ? "Collapse" : "Expand"}</button>
            <button aria-live="polite" title={copyFailed ? "Select and copy the visible command manually." : undefined} onClick={copyCommand}>{copied ? <Check /> : <Copy />}{copied ? "Copied" : copyFailed ? "Copy failed" : "Copy"}</button>
          </footer>
        </main>
      </div>
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
                {downloadPlan.partConflict && <p>Resume is not supported. To avoid deleting another running process's data, samloader will not replace <code>{downloadPlan.partPath}</code>. Confirm no download is using it, remove it manually, then prepare the download again.</p>}
                {downloadPlan.conflict && <label className="replace-confirm"><input type="checkbox" checked={allowReplace} onChange={(event) => setAllowReplace(event.target.checked)} /> Replace the existing completed file</label>}
              </div>
            )}
            <div className="modal-actions">
              <button ref={downloadCancelRef} className="ghost" onClick={() => { setDownloadPlan(null); setDownloadState("idle"); }}>Cancel</button>
              <button className="primary" disabled={!canStartDownloadPlan(downloadPlan.enoughSpace, downloadPlan.conflict, downloadPlan.partConflict, allowReplace)} onClick={startDownload}>Start download</button>
            </div>
          </div>
        </div>
      )}

      {flashModal && (
        <div className="modal-backdrop">
          <div className="modal flash-review" role="dialog" aria-modal="true" aria-labelledby="flash-review-title" aria-describedby="flash-review-description">
            <div className="row">
              <div className={destructive ? "modal-icon danger" : "modal-icon"}>{destructive ? <WarningOctagon weight="fill" /> : <Lightning weight="fill" />}</div>
              <h2 id="flash-review-title">{destructive ? "Confirm destructive flash" : "Confirm flash"}</h2>
            </div>
            <p id="flash-review-description">Review every detail. Firmware for the wrong model, bootloader, or region can permanently damage the device.</p>
            <div className="modal-list">
              <ModalItem label="Mode" value={flashMode === "folder" ? "Firmware folder" : "Package files"} />
              <ModalItem label="CSC" value={flashMode === "folder" ? `${cscLabel(cscMode)} · ${cscMode === "wipe" ? "factory reset" : "keep data"}` : basename(packages.csc ?? "not selected")} />
              <ModalItem label="Packages" value={selectedPackages.map(basename).join(", ")} />
              <ModalItem label="Integrity" value={unverifiedTarPackages.length > 0 ? `${unverifiedTarPackages.length} plain .tar package${unverifiedTarPackages.length === 1 ? "" : "s"} cannot be MD5-verified` : flashOpts.skipMd5 ? "MD5 verification disabled" : "MD5 footer verification enabled"} />
              <ModalItem label="Device" value={deviceConnected ? deviceMessage : flashOpts.wait ? "not connected · cancellable waiting enabled" : "disconnected · return and enable waiting"} />
              {flashOpts.repartition && <ModalItem label="PIT" value="Repartition enabled · BL, AP, CP, and regular CSC selected" />}
            </div>
            {destructive && <div className="warning-row"><WarningOctagon />{flashOpts.repartition ? "Repartition rewrites the device partition table and can brick an incompatible device." : "The selected regular CSC performs a factory reset and erases user data."}</div>}
            {unverifiedTarPackages.length > 0 && <div className="warning-row"><Warning /> Plain .tar packages have no MD5 footer. Their integrity cannot be verified before flashing.</div>}
            {(flashOpts.skipMd5 || flashOpts.skipSize) && <div className="inline-error" role="alert"><WarningOctagon /> Protection disabled: {[flashOpts.skipMd5 && "MD5 verification", flashOpts.skipSize && "partition size checks"].filter(Boolean).join(" and ")}.</div>}
            <label className="flash-acknowledge"><input type="checkbox" checked={flashAcknowledged} onChange={(event) => setFlashAcknowledged(event.target.checked)} /> I verified that these packages match the model and bootloader of the device I intend to flash.</label>
            {flashPhrase && (
              <Field label={`Type ${flashPhrase} to continue`}>
                <input className="danger-confirm-input" value={flashConfirmation} onChange={(event) => setFlashConfirmation(event.target.value)} autoComplete="off" spellCheck={false} />
              </Field>
            )}
            <div className="modal-actions">
              <button ref={flashCancelRef} className="ghost" onClick={() => { setFlashModal(false); setFlashAcknowledged(false); setFlashConfirmation(""); }}>Cancel</button>
              <button className={destructive ? "primary danger" : "primary"} disabled={!flashConfirmationValid || !flashReady} onClick={confirmFlash}>{deviceConnected ? flashOpts.wait ? "Check device & flash" : "Flash now" : flashOpts.wait ? "Wait for device & flash" : "Device disconnected"}</button>
            </div>
          </div>
        </div>
      )}

      {closeBlocked && (
        <div className="modal-backdrop">
          <div className="modal close-blocked" role="alertdialog" aria-modal="true" aria-labelledby="close-blocked-title" aria-describedby="close-blocked-description">
            <div className="row">
              <div className={closeBlocked === "flash" ? "modal-icon danger" : "modal-icon"}><WarningOctagon weight="fill" /></div>
              <div><h2 id="close-blocked-title">Keep samloader open</h2><p id="close-blocked-description">{closeBlockedMessage || (closeBlocked === "flash" ? "Closing now would interrupt a device flash and may leave the device unusable." : closeBlocked === "flash-wait" ? "The app is waiting for a device. Cancel waiting before you close it." : closeBlocked === "device" ? "A device operation is still finishing. Keep the app open and try again when it completes." : "The firmware download must finish or be cancelled and cleaned up before closing.")}</p></div>
            </div>
            <div className="modal-actions">
              <button ref={closeCancelRef} className="ghost" onClick={() => { setCloseBlocked(null); if (operationView) setView(operationView); }}>Keep app open</button>
              {closeBlocked === "flash-wait" && <button className="ghost danger-outline" onClick={cancelFlashWait}><X /> Cancel waiting</button>}
              {closeBlocked === "download" && <button className="ghost danger-outline" disabled={downloadState === "canceling" || downloadControlBusy} onClick={() => { setCloseBlocked(null); void controlDownload("cancel_download"); }}><X /> {downloadState === "canceling" ? "Stopping..." : "Cancel download"}</button>}
              {closeBlocked === "flash" && <button className="primary" disabled>Flash must finish</button>}
              {closeBlocked === "device" && <button className="primary" disabled>Device operation must finish</button>}
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

function NavButton({ icon, label, active, onClick, disabled }: { icon: React.ReactNode; label: string; active: boolean; onClick: () => void; disabled?: boolean }) {
  return <button className={`nav-button ${active ? "active" : ""}`} disabled={disabled} aria-label={label} title={label} aria-current={active ? "page" : undefined} onClick={onClick}>{icon}<span>{label}</span></button>;
}

function StatusDot({ connected, busy }: { connected: boolean; busy: boolean }) {
  return <span aria-hidden="true" className={`status-dot ${connected ? "ok" : ""} ${busy ? "busy" : ""}`} />;
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

function Switch({ on, onClick, label, disabled }: { on: boolean; onClick: () => void; label: string; disabled?: boolean }) {
  return <button className={`switch ${on ? "on" : ""}`} type="button" role="switch" aria-label={label} aria-checked={on} disabled={disabled} onClick={onClick}><span /></button>;
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

export function flashReadinessMessage(
  selectedCount: number,
  scanning: boolean,
  connected: boolean,
  waitEnabled: boolean,
  repartition: boolean,
  missingSlots: readonly string[],
  repartitionHasWipeCsc = true,
  folderReviewValid = true,
) {
  if (scanning) return "Wait for the firmware folder scan to finish.";
  if (selectedCount === 0) return "Select at least one firmware package.";
  if (!folderReviewValid) return "Scan the firmware folder again before review.";
  if (repartition && missingSlots.length > 0) return `Repartition requires ${missingSlots.map((slot) => slot.toUpperCase()).join(", ")}.`;
  if (repartition && !repartitionHasWipeCsc) return "Repartition requires regular CSC (factory reset); HOME_CSC cannot be used.";
  if (!connected && !waitEnabled) return "Connect a Download Mode device or enable Wait for device.";
  return "Ready to review flash.";
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
