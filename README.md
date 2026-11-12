# QuickByte (Rust)

The Rust/egui build of QuickByte. It is the same download manager as the C# build in `../src`,
rewritten for speed and a smaller footprint, with a new compact interface. `MIGRATION.md` explains
the design, what stays compatible, and how far the migration has got.

## Layout

```
crates/
  quickbyte-core/    the engine (no UI): protocols, segmentation, merging, queues, persistence
  quickbyte-shell/   Windows integration: single instance, Run keys, wake task, tray, taskbar
  quickbyte-ui/      the egui app -> QuickByte.exe
  quickbyte-agent/   the queue scheduler -> QuickByte.Agent.exe
  quickbyte-cli/     the command line -> qb.exe
installer/           Inno Setup script (same AppId as the C# installer: upgrades in place)
scripts/             build-release.ps1
site/                the website (Cloudflare Pages); site/releases/ is written by build-release.ps1
```

## Building

You need the stable Rust toolchain (`x86_64-pc-windows-msvc`) and, to make the installer,
Inno Setup 6.

```
cargo build --workspace                  # debug build
cargo run -p quickbyte-ui -- --demo      # the UI with in-memory demo data; touches nothing on the machine
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

### Running a dev build on a machine with QuickByte installed

A plain `cargo run -p quickbyte-ui` uses your real profile, hands its launch to the installed copy
if that one is running (same mutex), and re-asserts your real Start-with-Windows entry. Use the
sandbox instead:

```
$env:QUICKBYTE_SANDBOX = "$env:TEMP\qb-sandbox"; cargo run -p quickbyte-ui
```

In the sandbox the data folder is that directory, the mutex and pipe names carry a `.sandbox`
suffix, and nothing is written to the registry or Task Scheduler. The browser bridge still listens on
the port in the sandbox's settings, so it fails to bind while the installed copy holds 9614.

The CRT is linked statically (`.cargo/config.toml`), so the executables run on a clean Windows 10
or 11 machine with nothing else installed.

## Releasing

The version lives only in `[workspace.package] version` in `Cargo.toml`.

```
./scripts/build-release.ps1 -ReleaseNotes "…" [-PublishToSite]
```

This builds in release mode, stages `QuickByte.exe`, `QuickByte.Agent.exe` and `qb.exe`, compiles
`installer/QuickByte.iss` into `installer/Output/QuickByte-<version>-Setup.exe`, and writes
`latest.json` with the installer's size and SHA-256. With `-PublishToSite`, it also copies both
files into `site/releases/` (removing older installers there), so deploying the website publishes the update. Copies already
installed, including the C# 1.26 builds, are then offered the update at their next launch.

A pre-release label or a build number can be set without editing anything:
`$env:QB_VERSION_SUFFIX = "beta.1"`, `$env:QB_BUILD_REVISION = "137"`.

## Your data

Everything lives where the C# build kept it, in `%APPDATA%\QuickByte`: `settings.json`,
`downloads.json`, `queues.json` and the `temp` chunk folder. An upgrade carries every download,
queue, setting, saved password and the browser pairing token across unchanged. The files stay
readable by the C# build, so rolling back is safe as well.
