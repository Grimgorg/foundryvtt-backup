# Build plan

How FoundryVTT Backup is built, tested and released for macOS, Windows and Linux.

## 1. Tech stack

| Part | Choice | Why |
|---|---|---|
| App shell | [Tauri 2](https://v2.tauri.app) | Small native apps (~5–15 MB) for all three OSes from one codebase. Uses the OS web view. |
| Backend | Rust (`src-tauri/src`) | All file work: zipping, extracting, lock detection, settings. |
| Frontend | Vite + vanilla TypeScript + CSS (`src/`, `index.html`) | No framework needed for a single screen. |
| Zip | `zip` crate (deflate, zip64) | Standard `.zip` files you can also open by hand. |
| CI/CD | GitHub Actions + `tauri-apps/tauri-action` | Builds every OS on its own runner. |

## 2. What gets shipped ("one file per OS")

A single binary can't run on all three operating systems, so each OS gets its own self-contained file:

| OS | Main file | Also attached |
|---|---|---|
| Windows 10/11 (x64) | `FoundryVTT-Backup_<ver>_windows_x64_portable.exe`: one file, no install | NSIS installer `*-setup.exe` |
| macOS 10.15+ (Apple Silicon + Intel) | `FoundryVTT.Backup_<ver>_universal.dmg` | `*.app.tar.gz` |
| Linux x64 | `FoundryVTT.Backup_<ver>_amd64.AppImage`: one file, `chmod +x` and run | `.deb` |

The portable Windows exe uses the Microsoft Edge WebView2 runtime. It is preinstalled on Windows 11 and on up-to-date Windows 10. The installer can install WebView2 itself if it is missing.

## 3. Local development

Prerequisites:
- Node.js 20+ and npm
- Rust (stable, 1.90+) via [rustup](https://rustup.rs)
- OS packages, see the [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/):
  - macOS: Xcode Command Line Tools (`xcode-select --install`).
  - Windows: Microsoft C++ Build Tools and WebView2.
  - Linux (Debian/Ubuntu):
    `sudo apt install libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf build-essential`

Commands:

```bash
npm install
npm run tauri dev          # run the app with hot reload
cd src-tauri && cargo test # backend unit tests
npm run tauri build        # release build for the current OS
```

macOS universal build (Apple Silicon + Intel):

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
npm run tauri build -- --target universal-apple-darwin
```

Build output goes to `src-tauri/target/<target>/release/bundle/`.

## 4. Icon

`app-icon.svg` is the source. To regenerate every platform icon after changing it:

```bash
npm run icon   # renders app-icon.png + public/logo.png, then runs `tauri icon`
```

Delete the generated `src-tauri/icons/android` and `src-tauri/icons/ios` folders afterwards, since this is a desktop-only app.

## 5. Continuous integration (`.github/workflows/ci.yml`)

Runs on every push to `main` and on every pull request, on Ubuntu, macOS and Windows:
1. `npm ci` and `npm run build` (TypeScript type check and Vite build)
2. `cargo fmt --check`
3. `cargo clippy --all-targets -D warnings`
4. `cargo test`. This includes the backup→restore round trip, retention, zip-slip protection and the "world is open in Foundry" lock detection.

## 6. Releases (`.github/workflows/release.yml`)

1. Bump the version in **all three** files, which must match:
   - `package.json`
   - `src-tauri/tauri.conf.json`
   - `src-tauri/Cargo.toml`
2. Commit, then tag and push:
   ```bash
   git tag v0.2.0
   git push origin main v0.2.0
   ```
3. GitHub Actions builds macOS (universal), Windows and Linux in parallel. It creates a **draft** release `v0.2.0` with all files and adds the portable Windows exe.
4. Check the draft on GitHub (download, quick test), then click **Publish release**.

You can also start the workflow by hand ("Run workflow" in the Actions tab).

## 7. Code signing (optional, later)

The builds are **not signed** for now, which is fine for personal use.

- **macOS:** the first time, right-click the app → **Open** → **Open**. Or run `xattr -cr "/Applications/FoundryVTT Backup.app"`.
  To sign and notarize, add the `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD` and `APPLE_TEAM_ID` secrets and pass them as env to `tauri-action`. This needs an Apple Developer account.
- **Windows:** SmartScreen may warn the first time: **More info → Run anyway**. Signing needs a code-signing certificate (for example Azure Trusted Signing). See the [Tauri docs](https://v2.tauri.app/distribute/sign/windows/).
- **Linux:** no signing needed.

## 8. Roadmap ideas

- Scheduled/automatic backups (e.g. on app start or every N hours)
- Auto-updater (`tauri-plugin-updater`, needs signed update artifacts)
- Optional backup of `Data/modules` and `Data/systems`
- Warning when the target PC lacks the world's game system or modules
- Tray icon / run in background
