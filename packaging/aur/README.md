# AUR package maintenance

This PKGBUILD builds from the GitHub release tarball. Publishing/updating:

1. Tag and push the release (`vX.Y.Z`) so the tarball URL exists.
2. Update `pkgver` (and reset `pkgrel=1`), then refresh the checksum:
   `updpkgsums` (from pacman-contrib), or manually
   `curl -L https://github.com/mbeaman/astral-watch/archive/refs/tags/vX.Y.Z.tar.gz | sha256sum`.
3. Test locally: `makepkg -srci` (builds, runs `cargo test`, installs), then
   `namcap PKGBUILD astral-watch-*.pkg.tar.zst` for lint.
4. Regenerate metadata: `makepkg --printsrcinfo > .SRCINFO`.
5. Push `PKGBUILD` + `.SRCINFO` to the AUR git remote
   (`ssh://aur@aur.archlinux.org/astral-watch.git`).

Post-install steps users need (worth a comment on the AUR page): the `i2c` group and
`astral-watch` user come from sysusers.d automatically on install; enable with
`sudo systemctl enable --now astral-watch`. The config lives at `/etc/astral-watch.toml`
(pacman `backup=` protects local edits).
