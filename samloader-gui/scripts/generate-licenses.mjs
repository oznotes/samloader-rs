import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { basename, dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const guiRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repositoryRoot = resolve(guiRoot, "..");
const licenseDirectory = join(guiRoot, "public", "licenses");

function writeIfChanged(path, content) {
  try {
    if (readFileSync(path, "utf8") === content) return;
  } catch {
    // A missing generated file is created below.
  }
  writeFileSync(path, content, "utf8");
}

function copyText(source, destination) {
  writeIfChanged(join(licenseDirectory, destination), readFileSync(source, "utf8"));
}

function inline(value) {
  return String(value).replaceAll("`", "'").replaceAll(/\s+/g, " ").trim();
}

function npmPackageName(packagePath, info) {
  if (info.name) return info.name;
  const normalized = packagePath.replaceAll("\\", "/");
  const marker = normalized.lastIndexOf("node_modules/");
  const parts = normalized.slice(marker + "node_modules/".length).split("/");
  return parts[0].startsWith("@") ? `${parts[0]}/${parts[1]}` : parts[0];
}

const LICENSE_FILE = /^(?:licen[cs]es?|copying|copyrights?|notices?|unlicense)(?:$|[._-])/i;

function collectLicensePaths(packageDirectory, explicitPaths = []) {
  const paths = new Set();
  if (existsSync(packageDirectory)) {
    for (const entry of readdirSync(packageDirectory, { withFileTypes: true })) {
      const path = join(packageDirectory, entry.name);
      if ((entry.isFile() || entry.isSymbolicLink()) && LICENSE_FILE.test(entry.name)) {
        paths.add(path);
      } else if (entry.isDirectory() && /^(?:licenses?|legal)$/i.test(entry.name)) {
        for (const child of readdirSync(path, { withFileTypes: true })) {
          if (child.isFile() || child.isSymbolicLink()) paths.add(join(path, child.name));
        }
      }
    }
  }
  for (const explicitPath of explicitPaths) {
    if (!explicitPath) continue;
    const resolvedPath = isAbsolute(explicitPath) ? explicitPath : join(packageDirectory, explicitPath);
    if (existsSync(resolvedPath)) paths.add(resolvedPath);
  }
  return [...paths].sort((left, right) => left.localeCompare(right));
}

function normalizeLicenseText(path) {
  const content = readFileSync(path, "utf8")
    .replaceAll("\r\n", "\n")
    .replaceAll("\r", "\n")
    .split("\n")
    .map((line) => line.trimEnd())
    .join("\n")
    .trimEnd();
  if (!content || content.includes("\u0000")) return null;
  return `${content}\n`;
}

function html(value) {
  return String(value).replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;");
}

const documents = new Map();

function registerDocument(content, filename, packageLabel) {
  const hash = createHash("sha256").update(content).digest("hex");
  if (!documents.has(hash)) {
    documents.set(hash, { content, filenames: new Set(), packages: new Set() });
  }
  const document = documents.get(hash);
  document.filenames.add(filename);
  document.packages.add(packageLabel);
  return hash;
}

function attachLicenseDocuments(pkg, packageDirectory, explicitPaths = []) {
  const packageLabel = `${pkg.ecosystem}:${pkg.name}@${pkg.version}`;
  const hashes = [];
  for (const path of collectLicensePaths(packageDirectory, explicitPaths)) {
    const content = normalizeLicenseText(path);
    if (!content) continue;
    hashes.push(registerDocument(content, basename(path), packageLabel));
  }
  return { ...pkg, documents: [...new Set(hashes)].sort() };
}

function repositoryUrl(value) {
  const raw = typeof value === "string" ? value : value?.url;
  return raw ? raw.replace(/^git\+/, "").replace(/^git:\/\//, "https://").replace(/\.git$/, "").replace(/\/$/, "") : null;
}

function authorNames(author, contributors = []) {
  return [author, ...contributors]
    .flat()
    .filter(Boolean)
    .map((entry) => typeof entry === "string" ? entry : entry.name ?? entry.email ?? JSON.stringify(entry));
}

mkdirSync(licenseDirectory, { recursive: true });
copyText(join(repositoryRoot, "LICENSE"), "LICENSE-Apache-2.0.txt");
copyText(join(repositoryRoot, "LICENSE-MIT"), "LICENSE-MIT.txt");
copyText(join(repositoryRoot, "THIRD_PARTY_NOTICES.md"), "THIRD_PARTY_NOTICES.md");

const cargoMetadata = JSON.parse(execFileSync(
  "cargo",
  ["metadata", "--locked", "--format-version", "1", "--filter-platform", "x86_64-pc-windows-msvc"],
  { cwd: repositoryRoot, encoding: "utf8", maxBuffer: 32 * 1024 * 1024 },
));
const rustPackages = cargoMetadata.packages
  .filter((pkg) => pkg.source !== null)
  .map((pkg) => attachLicenseDocuments(
    {
      ecosystem: "cargo",
      name: pkg.name,
      version: pkg.version,
      license: pkg.license,
      scope: "runtime/build",
      authors: pkg.authors ?? [],
      repository: repositoryUrl(pkg.repository),
    },
    dirname(pkg.manifest_path),
    [pkg.license_file],
  ))
  .sort((left, right) => left.name.localeCompare(right.name) || left.version.localeCompare(right.version));

const packageLock = JSON.parse(readFileSync(join(guiRoot, "package-lock.json"), "utf8"));
const npmPackages = Object.entries(packageLock.packages)
  .filter(([packagePath]) => packagePath !== "" && existsSync(join(guiRoot, packagePath)))
  .map(([packagePath, info]) => {
    const packageDirectory = join(guiRoot, packagePath);
    let manifest = {};
    try {
      manifest = JSON.parse(readFileSync(join(packageDirectory, "package.json"), "utf8"));
    } catch {
      // Missing package manifests are rejected through the metadata check below.
    }
    return attachLicenseDocuments({
      ecosystem: "npm",
      name: npmPackageName(packagePath, { ...info, name: manifest.name }),
      version: info.version ?? manifest.version,
      license: info.license ?? manifest.license,
      scope: info.dev ? "development" : "runtime",
      authors: authorNames(manifest.author, manifest.contributors),
      repository: repositoryUrl(manifest.repository),
    }, packageDirectory, [manifest.licenseFile, manifest.license_file]);
  })
  .filter((pkg) => pkg.name && pkg.version)
  .sort((left, right) => left.name.localeCompare(right.name) || left.version.localeCompare(right.version));

const missingLicenses = [...rustPackages, ...npmPackages].filter((pkg) => !pkg.license);
if (missingLicenses.length > 0) {
  throw new Error(`Missing dependency license metadata: ${missingLicenses.map((pkg) => `${pkg.name}@${pkg.version}`).join(", ")}`);
}

const allPackages = [...rustPackages, ...npmPackages];
const repositoryDocuments = new Map();
for (const pkg of allPackages) {
  if (!pkg.repository || pkg.documents.length === 0) continue;
  const key = pkg.repository.toLowerCase();
  if (!repositoryDocuments.has(key)) repositoryDocuments.set(key, new Set());
  for (const hash of pkg.documents) repositoryDocuments.get(key).add(hash);
}

const mitBody = readFileSync(join(repositoryRoot, "LICENSE-MIT"), "utf8")
  .replaceAll("\r\n", "\n")
  .replace(/^(?:Copyright[^\n]*\n)+\n?/, "")
  .trim();
const apacheText = `${readFileSync(join(repositoryRoot, "LICENSE"), "utf8").replaceAll("\r\n", "\n").trimEnd()}\n`;
const mplPackage = allPackages.find((pkg) => pkg.license === "MPL-2.0" && pkg.documents.length > 0);

for (const pkg of allPackages.filter((candidate) => candidate.documents.length === 0)) {
  const packageLabel = `${pkg.ecosystem}:${pkg.name}@${pkg.version}`;
  const attribution = pkg.authors.length > 0
    ? pkg.authors.join(", ")
    : `the contributors to ${pkg.name}${pkg.repository ? ` (${pkg.repository})` : ""}`;
  const repositoryHashes = pkg.repository ? repositoryDocuments.get(pkg.repository.toLowerCase()) : null;
  if (repositoryHashes) {
    for (const hash of repositoryHashes) {
      documents.get(hash).packages.add(packageLabel);
      pkg.documents.push(hash);
    }
  }

  if (/\bMIT\b/.test(pkg.license)) {
    const content = `Copyright attribution: ${attribution}\n\n${mitBody}\n`;
    pkg.documents.push(registerDocument(content, "MIT-with-package-attribution.txt", packageLabel));
  }
  if (/Apache-2\.0/.test(pkg.license)) {
    pkg.documents.push(registerDocument(apacheText, "Apache-2.0.txt", packageLabel));
  }
  if (/MPL-2\.0/.test(pkg.license) && mplPackage) {
    for (const hash of mplPackage.documents) {
      documents.get(hash).packages.add(packageLabel);
      pkg.documents.push(hash);
    }
  }
  if (/BSD-3-Clause/.test(pkg.license)) {
    const canonical = allPackages.find((candidate) => candidate.license === "BSD-3-Clause" && candidate.documents.length > 0);
    if (canonical) {
      for (const hash of canonical.documents) {
        documents.get(hash).packages.add(packageLabel);
        pkg.documents.push(hash);
      }
    }
  }

  const attributionRecord = [
    "Dependency attribution record",
    `Package: ${pkg.name}`,
    `Version: ${pkg.version}`,
    `Declared license: ${pkg.license}`,
    `Published authors/contributors: ${attribution}`,
    `Repository: ${pkg.repository ?? "not declared"}`,
    "The installed package archive did not include its own top-level license or notice file; canonical license text and published attribution metadata are preserved here.",
    "",
  ].join("\n");
  pkg.documents.push(registerDocument(attributionRecord, "GENERATED-ATTRIBUTION.txt", packageLabel));
  pkg.documents = [...new Set(pkg.documents)].sort();
}

const missingTexts = allPackages.filter((pkg) => pkg.documents.length === 0);
if (missingTexts.length > 0) {
  throw new Error(`Missing dependency license or notice text: ${missingTexts.map((pkg) => `${pkg.ecosystem}:${pkg.name}@${pkg.version}`).join(", ")}`);
}

const documentHashes = [...documents.keys()].sort();
const documentIds = new Map(documentHashes.map((hash, index) => [hash, `D${String(index + 1).padStart(4, "0")}`]));

function packageLine(pkg) {
  const references = pkg.documents.map((hash) => `[${documentIds.get(hash)}](#${documentIds.get(hash).toLowerCase()})`).join(", ");
  return `- \`${inline(pkg.name)} ${inline(pkg.version)}\` — ${inline(pkg.license)} (${pkg.scope}); texts: ${references}`;
}

const lines = [
  "# Dependency license inventory",
  "",
  "Generated from the locked Windows x64 dependency graph. Regenerate with `npm run licenses`.",
  "This file preserves the declared SPDX expressions and the actual LICENSE, COPYING, COPYRIGHT, NOTICE, and UNLICENSE texts shipped by every installed dependency. Identical documents are deduplicated without removing package attribution.",
  "Project, Heimdall-derived MIT, and font license texts are also distributed beside this file.",
  "",
  `## Rust crates (${rustPackages.length})`,
  "",
  ...rustPackages.map(packageLine),
  "",
  `## npm packages (${npmPackages.length})`,
  "",
  ...npmPackages.map(packageLine),
  "",
  `## Preserved license and notice texts (${documentHashes.length})`,
  "",
];

for (const hash of documentHashes) {
  const document = documents.get(hash);
  const id = documentIds.get(hash);
  lines.push(
    `### ${id}`,
    "",
    `Packages: ${[...document.packages].sort().map((pkg) => `\`${inline(pkg)}\``).join(", ")}`,
    "",
    `Source filenames: ${[...document.filenames].sort().map((name) => `\`${inline(name)}\``).join(", ")}`,
    "",
    `<pre>${html(document.content)}</pre>`,
    "",
  );
}

writeIfChanged(join(licenseDirectory, "DEPENDENCY_LICENSES.md"), lines.join("\n"));
