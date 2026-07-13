# Public release checklist

The Windows desktop release is public only after every item below passes.

## Automated gates

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- `cargo test --release --workspace`
- `cargo audit` with every vulnerability-class finding resolved; informational
  unmaintained notices are reviewed and their transitive owner is recorded.
  New unsoundness warnings fail the gate. `RUSTSEC-2024-0429` is the single
  scoped exception: it is in Tauri's Linux-only GTK dependency tree and
  `cargo tree --target x86_64-pc-windows-msvc -i glib@0.18.5` confirms it is
  not linked into the public Windows artifacts
- `npm ci`, `npm audit --audit-level=high`, `npm test`, and `npm run build`
- Signed Tauri installer, portable executable, and CLI executable all report a
  valid Authenticode signature
- `SHA256SUMS.txt` contains every public executable
- A CycloneDX SBOM and GitHub build-provenance attestation are generated
- The release tag exactly matches the desktop version
- No GitHub release already exists for the exact tag

The `Publish signed Windows desktop release` workflow enforces these checks and
will not publish when signing secrets are absent. Configure its
`windows-release` GitHub environment with required reviewers, restrict release
tag creation to maintainers, and protect the `main` branch.

Configure repository secrets before creating a release tag:

- `WINDOWS_CERTIFICATE`: the base64-encoded contents of the public code-signing
  `.pfx` file;
- `WINDOWS_CERTIFICATE_PASSWORD`: the `.pfx` export password.

The certificate must be valid for code signing and unexpired. Push a tag that
exactly matches the desktop version, for example `vX.Y.Z`; a successful tagged
workflow publishes the signed assets. `workflow_dispatch` builds and uploads a
signed workflow artifact without creating a public GitHub release.

Current RustSec informational ownership review:

- GTK3/`proc-macro-error` maintenance notices are pulled by Tauri's Linux GUI
  tree and are not present in the Windows target dependency graph.
- The five `unic-*` maintenance notices are pulled by Tauri's current
  `urlpattern` dependency. They are maintenance notices, not vulnerability or
  unsoundness findings; Dependabot remains enabled so the chain is replaced as
  soon as Tauri updates it.

Published release tags are immutable. The workflow checks that the exact tag
does not already have a GitHub release and fails closed if it exists or if the
absence check cannot be completed. It never uses asset clobbering; corrections
require a new version and tag so stale or mixed assets cannot survive a rerun.
Assets are uploaded to a private draft, and that draft is made public only
after the complete upload succeeds.

After recording a successful physical-device matrix, set repository variables
`PUBLIC_RELEASE_APPROVED_VERSION` to the exact version and
`PUBLIC_RELEASE_APPROVED_SHA` to the exact tested commit. Tagged publishing
fails closed when either approval is absent or refers to another build.

## Required physical-device matrix

- ADB: no device, unauthorized device, offline device, one device, and multiple
  devices
- Download Mode: no device, one device on VCOM, one device on NUSB, and multiple
  compatible devices
- Firmware: latest stable, historical stable, unavailable beta, pause/resume,
  cancel, retry, low disk, destination conflict, and a completed archive
  integrity verification
- Flash: HOME_CSC, regular CSC factory-reset warning, wrong-model package,
  mixed-version package set, stored-member ZIP, compressed-member ZIP with
  sufficient and insufficient temporary space, temporary-workspace cleanup,
  duplicate partition, disconnect before session, disconnect during transfer,
  PIT read/dump, and no-reboot
- Repartition is tested only on dedicated recoverable hardware after a complete
  PIT backup and independent package/device compatibility review
- Attempting to close or start another device operation during a flash is
  blocked
- Single-instance activation restores and focuses the existing window; installer
  and portable binaries are not allowed to run concurrent device sessions
- Clean Windows 10 and Windows 11 virtual machines: installer install/launch,
  upgrade from the previous public version, uninstall, Start Menu integration,
  and launch as a standard (non-administrator) user
- Portable executable on a clean standard-user profile, including paths with
  spaces and non-ASCII characters
- WebView2 already installed, missing with a working network, and missing while
  offline; the embedded bootstrapper must explain or recover from each state
- Verify every staged filename, SHA-256 checksum, signer thumbprint, RFC 3161
  timestamp, SBOM, and provenance attestation from a freshly downloaded release

Record device models, CSCs, firmware versions, USB backend, driver version, and
test results with the release.
