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
- [ ] `nix flake update pinnacle` in the dotfiles and rebuild

## Not ours

- AUR: `pinnacle-comp` and `pinnacle-comp-git` are Ottatop's packages and track
  upstream, not this fork. Nothing to bump here unless a fork package is ever
  published.
- The `upstream` remote is local git config, not cloned state. On a fresh clone:
  `git remote add upstream https://github.com/pinnacle-comp/pinnacle` and
  `git remote set-url --push upstream DISABLED`.
