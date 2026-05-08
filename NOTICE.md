# virtiofsd macOS Fork Notice

This repository is maintained as `openresearchtools/virtiofsd`.

It is based on:

- `https://github.com/christhomas/virtiofsd`
- upstream virtiofsd: `https://gitlab.com/virtio-fs/virtiofsd`

Local fork changes include:

- macOS support carried from the `christhomas/virtiofsd` macOS branch
- defaulting `--inode-file-handles=never` on macOS so Apple-hosted shares do
  not require unsupported inode file-handle behavior
- GitHub Actions artifact packaging for the macOS binary and notices

Files added or changed by this fork for artifact packaging include:

- `src/main.rs`
- `.github/workflows/build-macos.yml`
- `scripts/package-macos-artifact.sh`
- `NOTICE.md`

License: `Apache-2.0 AND BSD-3-Clause`, as declared in `Cargo.toml` and the
bundled license files.
