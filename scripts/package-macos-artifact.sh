#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

BIN="${1:-$REPO_ROOT/target/release/virtiofsd}"
OUT_DIR="${2:-$REPO_ROOT/artifacts/virtiofsd-macos}"
PACKAGE_DIR="$OUT_DIR/package"

log() {
    printf '[package-virtiofsd] %s\n' "$*"
}

die() {
    printf '[package-virtiofsd] error: %s\n' "$*" >&2
    exit 1
}

[ -x "$BIN" ] || die "binary missing or not executable: $BIN"

rm -rf "$OUT_DIR"
mkdir -p "$PACKAGE_DIR"

log "copying binary and license files"
cp "$BIN" "$PACKAGE_DIR/virtiofsd"
cp "$REPO_ROOT/LICENSE-APACHE" "$PACKAGE_DIR/LICENSE-APACHE"
cp "$REPO_ROOT/LICENSE-BSD-3-Clause" "$PACKAGE_DIR/LICENSE-BSD-3-Clause"
cp "$REPO_ROOT/NOTICE.md" "$PACKAGE_DIR/NOTICE.md"
cp "$REPO_ROOT/README.md" "$PACKAGE_DIR/README.md"
cp "$REPO_ROOT/50-virtiofsd.json" "$PACKAGE_DIR/50-virtiofsd.json"

log "recording cargo dependency metadata"
cargo metadata --format-version 1 > "$PACKAGE_DIR/cargo-metadata.json"

python3 - "$PACKAGE_DIR/cargo-metadata.json" "$PACKAGE_DIR/THIRD_PARTY_NOTICES.md" <<'PY'
import json
import sys
from pathlib import Path

metadata = json.loads(Path(sys.argv[1]).read_text())
out = Path(sys.argv[2])
root = metadata.get("root_package", {}).get("id")

packages = []
for pkg in metadata.get("packages", []):
    if pkg.get("id") == root:
        continue
    packages.append(pkg)

packages.sort(key=lambda p: (p.get("name", ""), p.get("version", "")))

lines = [
    "# virtiofsd Third-Party Notices",
    "",
    "This artifact contains the `virtiofsd` binary built from",
    "`openresearchtools/virtiofsd`, based on `christhomas/virtiofsd` and",
    "upstream `https://gitlab.com/virtio-fs/virtiofsd`.",
    "",
    "The project license files are included next to this notice.",
    "",
    "## Rust crate dependency metadata",
    "",
    "The binary is built with Cargo. The generated `cargo-metadata.json` file",
    "is included in this artifact for exact dependency/version provenance.",
    "",
]

for pkg in packages:
    name = pkg.get("name", "")
    version = pkg.get("version", "")
    license_text = pkg.get("license") or pkg.get("license_file") or "UNKNOWN"
    repo = pkg.get("repository") or pkg.get("homepage") or ""
    lines.append(f"### {name} {version}")
    lines.append("")
    lines.append(f"- License metadata: `{license_text}`")
    if repo:
        lines.append(f"- Source: <{repo}>")
    lines.append("")

out.write_text("\n".join(lines))
PY

TARBALL="$OUT_DIR/virtiofsd-macos-arm64.tar.gz"
log "creating tarball"
tar -czf "$TARBALL" -C "$PACKAGE_DIR" .

(
    cd "$OUT_DIR"
    shasum -a 256 virtiofsd-macos-arm64.tar.gz > SHA256SUMS
)

rm -rf "$PACKAGE_DIR"
log "wrote artifact to $OUT_DIR"
