import { describe, expect, it } from "vitest";
import {
  canStartDownloadPlan,
  canRepartition,
  cscModeCliFlag,
  cscModeForFlashRequest,
  deviceCliSubcommand,
  detectedFirmwareIdentity,
  downloadOutputCliFlag,
  firmwareIdentityMatches,
  flashReadinessMessage,
  hasWipeCscForRepartition,
  isRegularCscPackage,
  missingRepartitionSlots,
  packagesForFlashRequest,
  powerShellQuote,
  readPreferences,
  validatePreferences,
  zipReviewValid,
} from "./App";

describe("download destination safety", () => {
  it("never auto-replaces a partial file that another process may own", () => {
    expect(canStartDownloadPlan(true, false, true, true)).toBe(false);
    expect(canStartDownloadPlan(true, true, false, false)).toBe(false);
    expect(canStartDownloadPlan(true, true, false, true)).toBe(true);
    expect(canStartDownloadPlan(false, false, false, false)).toBe(false);
  });

  it("never retries a version under a changed model or region", () => {
    expect(firmwareIdentityMatches("SM-S931B", "EUX", "sm-s931b", "eux")).toBe(true);
    expect(firmwareIdentityMatches("SM-S931B", "EUX", "SM-S938B", "EUX")).toBe(false);
    expect(firmwareIdentityMatches("SM-S931B", "EUX", "SM-S931B", "XAA")).toBe(false);
  });

  it("clears missing identity fields instead of retaining another device's values", () => {
    expect(detectedFirmwareIdentity({ model: " sm-s931b ", region: null }))
      .toEqual({ model: "SM-S931B", region: "" });
    expect(detectedFirmwareIdentity({ model: null, region: " xaa " }))
      .toEqual({ model: "", region: "XAA" });
  });
});

describe("PowerShell CLI rendering", () => {
  it("quotes shell metacharacters, variables, whitespace, and apostrophes literally", () => {
    expect(powerShellQuote("C:\\Firmware & Tools;$folder\\O'Brien.tar.md5"))
      .toBe("'C:\\Firmware & Tools;$folder\\O''Brien.tar.md5'");
  });

  it("quotes even simple placeholders consistently", () => {
    expect(powerShellQuote("XAA")).toBe("'XAA'");
  });

  it("uses the GUI download destination when the custom field is blank", () => {
    expect(downloadOutputCliFlag("", "C:\\Users\\consumer\\Downloads"))
      .toBe(" -d 'C:\\Users\\consumer\\Downloads'");
    expect(downloadOutputCliFlag(" D:\\Firmware ", "C:\\Downloads"))
      .toBe(" -d 'D:\\Firmware'");
  });

  it("keeps copied PIT inspection non-rebooting like the GUI", () => {
    expect(deviceCliSubcommand(true)).toBe("print-pit --no-reboot");
    expect(deviceCliSubcommand(false)).toBe("detect");
  });
});

describe("persisted preference validation", () => {
  it("rejects null and non-object storage payloads", () => {
    expect(validatePreferences(null)).toEqual({});
    expect(validatePreferences("bad")).toEqual({});
    expect(validatePreferences([])).toEqual({});
  });

  it("keeps valid fields and drops malformed values", () => {
    expect(validatePreferences({
      theme: "light",
      backend: "nusb",
      verbose: true,
      model: "SM-S931U1",
      region: "XAA",
      outDir: "C:\\Firmware",
      threads: 8,
      ignored: "value",
    })).toEqual({
      theme: "light",
      backend: "nusb",
      verbose: true,
      model: "SM-S931U1",
      region: "XAA",
      outDir: "C:\\Firmware",
      threads: 8,
    });
    expect(validatePreferences({ theme: "blue", threads: Number.NaN })).toEqual({});
  });

  it("returns empty preferences outside a browser", () => {
    expect(readPreferences()).toEqual({});
  });
});

describe("flash safety classification", () => {
  const complete = {
    bl: "BL_build.tar.md5",
    ap: "AP_build.tar.md5",
    cp: "CP_build.tar.md5",
    csc: "HOME_CSC_build.tar.md5",
  };
  const completeWithWipeCsc = { ...complete, csc: "CSC_build.tar.md5" };

  it("requires every core package before repartition", () => {
    expect(canRepartition(completeWithWipeCsc, false, "home")).toBe(true);
    expect(missingRepartitionSlots({ ...completeWithWipeCsc, cp: null })).toEqual(["cp"]);
    expect(canRepartition({ ...completeWithWipeCsc, cp: null }, false, "home")).toBe(false);
  });

  it("requires factory-reset CSC semantics for repartition", () => {
    expect(hasWipeCscForRepartition(false, complete, "wipe")).toBe(false);
    expect(hasWipeCscForRepartition(false, completeWithWipeCsc, "home")).toBe(true);
    expect(hasWipeCscForRepartition(true, completeWithWipeCsc, "home")).toBe(false);
    expect(hasWipeCscForRepartition(true, complete, "wipe")).toBe(true);
    expect(canRepartition(complete, false, "wipe")).toBe(false);
    expect(canRepartition(completeWithWipeCsc, true, "home")).toBe(false);
    expect(canRepartition(complete, true, "wipe")).toBe(true);
  });

  it("recognizes regular CSC packages as factory-reset packages", () => {
    expect(isRegularCscPackage("C:\\Firmware\\CSC_OXM_build.tar.md5")).toBe(true);
    expect(isRegularCscPackage("C:\\Firmware\\HOME_CSC_OXM_build.tar.md5")).toBe(false);
    expect(isRegularCscPackage("AP_build.tar.md5")).toBe(false);
  });

  it("sends one authoritative package source to the backend", () => {
    expect(packagesForFlashRequest(true, complete)).toEqual({});
    expect(packagesForFlashRequest(false, complete)).toEqual(complete);
    expect(packagesForFlashRequest(false, complete)).not.toBe(complete);
  });

  it("derives manual CSC mode from the selected package instead of stale folder state", () => {
    expect(cscModeForFlashRequest(false, "CSC_OXM_build.tar.md5", "home")).toBe("wipe");
    expect(cscModeForFlashRequest(false, "HOME_CSC_OXM_build.tar.md5", "wipe")).toBe("home");
    expect(cscModeForFlashRequest(true, "HOME_CSC_OXM_build.tar.md5", "wipe")).toBe("wipe");
    expect(cscModeCliFlag(false, "CSC_OXM_build.tar.md5", "home")).toBe(" --csc-mode wipe");
    expect(cscModeCliFlag(false, "HOME_CSC_OXM_build.tar.md5", "wipe")).toBe("");
  });

  it("explains why a flash is blocked", () => {
    expect(flashReadinessMessage(0, false, true, false, false, []))
      .toBe("Select at least one firmware package.");
    expect(flashReadinessMessage(4, false, true, false, true, ["cp"]))
      .toBe("Repartition requires CP.");
    expect(flashReadinessMessage(4, false, true, false, true, [], false))
      .toBe("Repartition requires regular CSC (factory reset); HOME_CSC cannot be used.");
    expect(flashReadinessMessage(4, false, false, false, false, []))
      .toBe("Connect a Download Mode device or enable Wait for device.");
  });
});

describe("zip flash readiness", () => {
  const scan = {
    packages: { ap: "AP_x.tar.md5", csc: "HOME_CSC_x.tar.md5" },
    identity: { path: "C:\\fw.zip", canonicalPath: "C:\\fw.zip", size: 10, modifiedUnixNanos: "1" },
    nonStored: [] as string[],
  };

  it("requires a scanned ZIP with only stored members", () => {
    expect(zipReviewValid("zip", null)).toBe(false);
    expect(zipReviewValid("zip", scan)).toBe(true);
    expect(zipReviewValid("zip", { ...scan, nonStored: ["AP_x.tar.md5"] })).toBe(false);
    expect(zipReviewValid("folder", null)).toBe(true);
    expect(zipReviewValid("pkg", null)).toBe(true);
  });

  it("prompts for a ZIP, then a scan, before it is ready", () => {
    expect(flashReadinessMessage(0, false, true, false, false, [], true, true, "zip", false, null))
      .toBe("Choose a firmware ZIP.");
    expect(flashReadinessMessage(0, false, true, false, false, [], true, true, "zip", true, null))
      .toBe("Scan the ZIP to review its packages.");
    expect(flashReadinessMessage(2, false, true, false, false, [], true, true, "zip", true, scan))
      .toBe("Ready to review flash.");
  });

  it("explains compressed members with an extract fallback", () => {
    const message = flashReadinessMessage(
      2, false, true, false, false, [], true, true,
      "zip", true, { ...scan, nonStored: ["AP_x.tar.md5"] },
    );
    expect(message).toContain("compressed");
    expect(message.toLowerCase()).toContain("extract");
    expect(message).toContain("Firmware folder mode instead");
  });
});
