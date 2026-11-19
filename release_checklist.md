# Release checklist

The fork is the project of record now, so this file is the release process.
Fork releases live on Codeberg; GitHub gets nothing but a mirror branch.

- [ ] Bump `version` in `Cargo.toml` (`[workspace.package]`)
- [ ] Run `cargo build` so `Cargo.lock` updates
- [ ] Bump `version` in `nix/packages/default.nix` and the `knownRockspec` it points at
- [ ] Copy `api/lua/rockspecs/pinnacle-api-<old>-1.rockspec` to the new version and bump it
- [ ] If the Rust API changed, bump `api/rust/Cargo.toml`
- [ ] `nix build .#pinnacle` (builds and runs the test suite)
- [ ] `git tag v<version>` and `git push origin main --tags`
- [ ] Codeberg release: attach the packed rock
      (`cd api/lua && luarocks build --lua-version 5.4 --deps-mode none --pack-binary-rock rockspecs/pinnacle-api-<version>-1.rockspec`)
- [ ] AUR: bump `pkgver`, `updpkgsums`, `makepkg --printsrcinfo > .SRCINFO`, update the
      `VERGEN_GIT_*` values the PKGBUILD feeds the build
- [ ] `nix flake update pinnacle` in the dotfiles and rebuild
