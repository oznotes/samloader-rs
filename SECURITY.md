# Security policy

Firmware flashing writes directly to device partitions and must be treated as a
high-impact operation. Please do not open a public issue for a vulnerability
that could expose users or devices before a fix is available.

Report security issues privately through GitHub's **Report a vulnerability**
feature for this repository. Include the affected version, operating system,
reproduction steps, and whether the issue can affect downloads, local files, or
connected devices.

Only the latest release receives security fixes. Release executables should be
downloaded from this repository's GitHub Releases page and verified against the
published `SHA256SUMS.txt`. Public Windows releases are Authenticode-signed; do
not redistribute binaries whose signature is missing or invalid. GitHub build
provenance can be verified with `gh attestation verify <artifact> -R <owner/repo>`.
