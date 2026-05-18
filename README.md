# dodev-public

Public release artifacts for the [do.dev](https://do.dev) ecosystem.

Source for the do.dev platform itself is in a private monorepo. This
repo exists so the public can:

- download release binaries without GitHub authentication
- view (and PR against) the open-source pieces that ship to end users

## What lives here

| Directory | Description |
|-----------|-------------|
| [`dodev-cli/`](./dodev-cli) | The `dodev` CLI — local.dev tunnels, account auth, self-update. Rust workspace. |

## Installing the CLI

```bash
# macOS / Linux
curl -fsSL https://local.dev/install.sh | sh

# Windows (PowerShell)
irm https://local.dev/install.ps1 | iex
```

Both redirect to the latest [GitHub Release](https://github.com/dodotdev/dodev-public/releases/latest).

Once installed:

```bash
dodev login                 # browser-based OAuth
dodev local http 3000       # expose localhost:3000 at https://<your-subdomain>.local.dev
dodev update                # self-update to latest
```

## Releasing

Tags shaped `dodev-cli-vX.Y.Z` trigger the cargo-dist workflow
(`.github/workflows/dodev-cli-release.yml`) which cross-compiles the
binary for 5 platforms and attaches the installer scripts to a fresh
GitHub Release.

```bash
# bump version in dodev-cli/cli/Cargo.toml first
git tag dodev-cli-v0.1.1 -m "release notes"
git push origin dodev-cli-v0.1.1
```

## License

Each subdirectory has its own license. See `dodev-cli/cli/Cargo.toml`
for the CLI (MIT OR Apache-2.0).
