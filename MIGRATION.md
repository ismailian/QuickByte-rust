# QuickByte: migrating from C# / WinForms to Rust / egui

This document is the plan and the record of the rewrite. It covers what is being
built, how it maps onto the C# solution, what must stay byte-compatible so that
an installed copy upgrades without the user noticing anything except the new UI,
and a milestone checklist that is ticked off as the work lands.

The C# solution under `src/` stays the reference implementation for the whole
migration. When this document and the C# code disagree about behaviour, the C#
code wins. When the C# `README.md` explains *why* something works the way it
does, that reasoning carries over unchanged, and the Rust code keeps the same
doc comments that explain it.

---

## 1. Goals and non-goals

### Goals

1. **Same product, new engine.** Every feature in the C# `README.md` is kept:
   segmented HTTP(S)/FTP(S) downloads with dynamic segmentation, connection
   recovery, per-connection control, the proven-range probe, `If-Range` file
   version tracking, the resumable merge and the disk-space check before the
   first byte, Mark of the Web, checksums, HLS/DASH, queues and the scheduler
   agent, wake timers, power actions, the browser bridge, `qb.exe`, the proxy,
   the custom User-Agent, clipboard watching, category folders, batch download,
   address refresh, the update checker, light and dark themes, and the tray.
2. **An upgrade the user does not notice.** A user on 1.26.x installs the next
   release, either by hand or from the in-app updater. Their downloads, queues,
   settings, encrypted passwords, browser pairing token, Run-key entries,
   wake task and partly downloaded chunks all carry on. The only visible change
   is the UI.
3. **Faster and lighter.** No .NET runtime: three self-contained native
   executables with the CRT statically linked, so there is no runtime or VC++
   redistributable to install. Lower idle CPU, lower memory, faster startup.
4. **Same architecture.** A headless core with no UI references, and thin
   front-ends (GUI, scheduler agent, CLI) that compose it. Contracts stay
   behind traits, so they can be swapped and faked in tests.
5. **A compact UI.** 12 px text everywhere, small controls, and a dense layout
   that follows the existing IDM-style window design.

### Non-goals

- **No new features** beyond what the C# code contains. **Known gap:** this
  repository is at commit `c32b573` (version 1.25.1). The installed 1.26.2
  build was made from newer, unpushed C# changes, including a VirusTotal
  feature. Once they are pushed, they are diffed against `c32b573` and ported
  into the matching Rust modules (milestone M9). Until then, the port
  **preserves** those keys in `settings.json` and the `VirusTotal` field on
  each download (see §5.6), so nothing is lost.
- **No changes to the browser extension.** It talks to the same loopback
  protocol on the same port with the same token, so it keeps working
  unmodified.
- **No change to the website or the update endpoint**, except publishing the
  new release's `latest.json`.

---

## 2. Version and release identity

| Item | C# today | Rust |
|---|---|---|
| Product version source | `Directory.Build.props` (repo says 1.25.1; installed build is 1.26.2) | `[workspace.package] version` in `rust/Cargo.toml`, the only place it is set |
| First Rust release | n/a | **1.27.0**. This must be strictly newer than the installed 1.26.2, or the old updater does not offer it and Inno Setup reports a downgrade. |
| Pre-release / build number | `-p:VersionSuffix` / `-p:BuildRevision` | `QB_VERSION_SUFFIX` / `QB_BUILD_REVISION` environment variables, read by each binary's `build.rs` and stamped into the Win32 `VERSIONINFO` resource |
| Version comparison | `ProductVersion` | `core::helpers::product_version`, ported together with its tests (it accepts a leading `v`, a missing patch number and `-prerelease`, and ranks a release above its own betas) |

Each `build.rs` embeds the same `VERSIONINFO` fields the C# assemblies carried.
Task Manager's Startup tab shows the `FileDescription`, so the values must
match exactly:

| Executable | FileDescription | Notes |
|---|---|---|
| `QuickByte.exe` | `QuickByte` | GUI subsystem; carries `quickbyte.ico` |
| `QuickByte.Agent.exe` | `QuickByte Queue Scheduler` | GUI subsystem (no console flash at sign-in) |
| `qb.exe` | `QuickByte Command Line` | console subsystem |

Cargo does not allow a `.` in a binary name, so the agent is built as
`quickbyte-agent.exe` and renamed to `QuickByte.Agent.exe` by the packaging
script. The app's agent lookup also accepts the cargo name, so a `cargo run`
dev build can find the agent too.

---

## 3. Solution layout

```
rust/
  Cargo.toml                 Workspace: the version, shared dependency versions, release profile
  MIGRATION.md               This document
  crates/
    quickbyte-core/          ↔ QuickByte.Core: all business logic, no UI and no egui
      src/
        enums.rs             DownloadStatus, ConnectionStatus, DownloadCategory, QueueState,
                               ScheduleDays, PowerAction, CompletionActions, AppTheme, ConnectionCommand
        models/              DownloadItem, DownloadSettings, ConnectionInfo, RemoteFileInfo,
                               DownloadQueue, QueueSchedule, ProxySettings, StreamSource, …
        events.rs            Event payloads (progress, status, connections, list, queues)
        error.rs             CoreError: one enum replacing the five exception types
        traits.rs            Every service contract (the C# Interfaces/ folder)
        helpers/             Pure logic: byte formatting, range splitting, speed, retry,
                               file names, bandwidth limiter, checksums, batch links, …
        net/                 HTTP client, proxy (AppProxy + ProxyTunnel), auth
                               (Basic/Digest/NTLM/Negotiate), request decoration
        ftp/                 FTP/FTPS client: control channel, info provider, connection
        services/            ConnectionPoolManager, DownloadService, StreamDownloadService,
                               DownloadManager, QueueManager, FileMerger, repositories,
                               SettingsService, UpdateService, BrowserIntegrationServer,
                               protocol dispatch
        streaming/           HLS/DASH parsers, StreamPlan, StreamResolver, StreamDetector
        commands/            qb grammar, pipe messages, formatter, processor, named pipe
        platform/            Win32 used by Core: DPAPI, disk space, MOTW stream,
                               current-user-only named pipes
    quickbyte-shell/         ↔ the non-visual half of QuickByte.UI: Windows integration
      src/                   single_instance, startup_registration, queue_agent_registration,
                               queue_wake_registration, power_actions, keep_awake,
                               taskbar_progress, dark_mode, tray (+ balloons), clipboard_watcher,
                               file_launcher, zone_information_policy, app_version
    quickbyte-ui/            ↔ QuickByte.UI forms: the egui application → QuickByte.exe
      src/
        main.rs              Composition root (the C# Program.cs)
        app.rs               Root eframe::App: owns the view models and the open windows
        dispatcher.rs        UiDispatcher: Core events → the UI thread + request_repaint
        theme.rs             Both palettes, 12 px type scale, compact spacing
        icons.rs             IconFactory/BrandIcon: every icon is drawn with egui's painter
        widgets/             progress bar, segments bar, flat tabs, connection-count box,
                               animator (ProgressAnimator<TKey>)
        windows/             main, details, new_download, batch, settings, queues, update,
                               complete, credentials, checksum, refresh_address, about,
                               power_countdown
    quickbyte-agent/         ↔ QuickByte.Agent → QuickByte.Agent.exe
    quickbyte-cli/           ↔ QuickByte.Cli → qb.exe
  installer/
    QuickByte.iss            Inno Setup 6 script. Same AppId as the C# installer (§6).
  scripts/
    build-release.ps1        cargo build --release, stage the binaries under their shipped names, compile the installer
```

### Mapping table (C# → Rust)

| C# | Rust | Notes |
|---|---|---|
| `QuickByte.Core` (class library) | `quickbyte-core` (lib crate) | Still has no UI references. The Win32 calls Core genuinely needs (DPAPI, the pipe ACL, ADS, free space) sit behind `platform/`. |
| `Interfaces/I*.cs` | `traits.rs` | `trait DownloadManager`, `trait ConnectionFactory`, … returning `BoxFuture`s (via `async-trait`) so they can be used as `Arc<dyn …>` and faked in tests |
| C# events + `IDispatcher` | `EventHub<T>` (observer) + `trait Dispatcher` | See §4.2 |
| `Task`, `CancellationToken` | tokio tasks, `tokio_util::sync::CancellationToken` | Same cancellation semantics, child tokens included |
| `Interlocked` counters | `AtomicU64` / `AtomicI64` | Connections still never raise per-byte events |
| `SemaphoreSlim` | `tokio::sync::Semaphore` | concurrent-download throttle |
| `System.Text.Json` | `serde` + `serde_json` | Field names and enum encodings stay identical (§5) |
| `HttpClient` / `SocketsHttpHandler` | `reqwest` (hyper) with **native-tls / SChannel** | One shared, pooled client, as before; SChannel uses the Windows certificate store as .NET did |
| `HttpClientHandler.Credentials` (Digest/NTLM/Negotiate) | Digest in Rust; NTLM and Negotiate through **SSPI** (`windows` crate) | Same rule: a login is sent only to the origin that asked for it |
| `TcpClient` FTP + `SslStream` | `tokio::net::TcpStream` + `tokio-native-tls` | |
| `ProtectedData` (DPAPI) | `CryptProtectData` / `CryptUnprotectData` | Same entropy and scope, so existing ciphertext still decrypts (§5.4) |
| `NamedPipeServerStream` (CurrentUserOnly) | `tokio::net::windows::named_pipe` + an explicit DACL naming only the current user's SID, plus a server-owner check on the client side | Same pipe names |
| `Mutex` (named) | `CreateMutexW` | Same names |
| `Microsoft.Win32.Registry` | `windows-registry` crate | In `quickbyte-shell` only, as before (no registry code in Core or the agent) |
| WinForms forms | egui windows (eframe viewports) | Each secondary window is a real OS window with its own taskbar button, as before |
| GDI+ owner-draw (`ListViewProgressPainter`, `ConnectionSegmentsBar`, `IconFactory`, `BrandIcon`) | `egui::Painter` | Icons are still generated from code; `quickbyte.ico` is still the one image file |
| `NotifyIcon` | `Shell_NotifyIconW` on a message-only window | That window also receives balloon clicks, clipboard updates and the taskbar-created broadcast |
| `ITaskbarList3` | Same COM interface through the `windows` crate | Progress on each details window's taskbar button |
| xUnit tests | `#[cfg(test)]` unit tests + `tests/` integration tests | Ported test by test. Fakes for the connection factory, info provider and repositories. |

---

## 4. Architecture

### 4.1 Layering (unchanged)

```
 quickbyte-ui (egui)     quickbyte-agent     quickbyte-cli
        │      │                │                  │
        │   quickbyte-shell     │                  │
        │      │                │                  │
        └──────┴───────► quickbyte-core ◄──────────┘
```

- `quickbyte-core` never depends on `quickbyte-shell`, egui or eframe.
  `cargo tree -p quickbyte-core` must never list `egui`, and CI checks this.
- The agent depends on Core only. It still has **no registry code**, so it can
  never register its own autostart.
- `quickbyte-shell` holds the Windows integrations, written with no egui
  types. Its non-visual logic is unit tested, just as the C# UI project's
  non-visual half was.

### 4.2 The download pipeline and the event model

The pipeline is the same six layers, with the same responsibilities:

1. `RemoteFileInfoProvider` (protocol dispatch → HTTP probe / FTP `SIZE`/`MDTM`/`FEAT`)
2. `DownloadManager`: the facade and registry, the persistence owner, and the concurrency semaphore
3. `DownloadService` / `StreamDownloadService`: one download's lifecycle state machine
4. `ConnectionPoolManager`: range splitting, dynamic segmentation, recovery,
   per-connection control, and throttled aggregate progress
5. `DownloadConnection` / `FtpDownloadConnection`: one byte range into one chunk file, with `RetryPolicy`
6. `FileMerger`: the resumable, delete-as-it-goes merge with `merge.json`

**Observer, made Rust-shaped.** C# raised .NET events from worker threads and
used `IDispatcher.Post` to marshal each one onto the UI thread. In the Rust
port:

- Each publisher owns an `EventHub<E>`, a list of subscribers, and
  `subscribe()` returns an `EventStream<E>` backed by an unbounded crossbeam
  channel. Publishing never blocks a download task.
- Inside the engine, layers listen to each other with **inline subscribers**
  (`EventHub::subscribe_fn`, and a `subscribe_fn` default method on the pool,
  service, manager and settings traits): the callback runs on the publishing
  task, in order, like a C# event handler — the service hearing its pool, the
  manager re-publishing its services, the queue manager hearing a removal.
  A trait object that only offers channels gets a draining thread by default.
- `trait Dispatcher { fn notify(&self); }` is what is left of `IDispatcher`. The
  UI's implementation calls `egui::Context::request_repaint()`. Each frame, the
  UI thread drains its streams, so every event is applied on the UI thread, in
  order, as before.
- The main window and every details window apply **the same event stream** to
  **one shared view model** keyed by download id. That keeps the C# guarantee
  that every open window shows identical numbers.
- Progress is still sampled by the pool manager's timer (default 100 ms,
  clamped to 50–2000), never per byte.

**State ownership.** In C#, the UI read the same mutable `DownloadItem` objects
that the engine wrote. In Rust, each service owns its item behind a
`parking_lot::Mutex`. `DownloadManager::downloads()` returns cloned snapshots,
and the UI keeps its own copies, which it updates from events. There are no
shared mutable references across threads.

### 4.3 Async runtime

- One multi-threaded tokio runtime, with `worker_threads = min(4, logical CPUs)`
  and named threads. Downloads are I/O-bound, and a small pool keeps the memory
  footprint low.
- Chunk writes use `std::fs::File` inside `spawn_blocking`-free buffered writers
  on the worker threads, as the C# `FileStream` did. Each connection keeps one
  reusable buffer (`StreamBufferSizeBytes`, clamped 4 KiB–8 MiB), so the hot
  loop allocates nothing.
- The UI thread never blocks on the runtime. It calls `handle.spawn(...)`, and
  results arrive as events, or as a `oneshot` the window polls each frame.

### 4.4 Errors

The five C# exception types become variants of one `CoreError` enum
(`thiserror`), so `match` can tell them apart where C# used `catch` filters:

| C# exception | `CoreError` variant | Retried by `RetryPolicy`? |
|---|---|---|
| `AuthenticationRequiredException { CredentialsWereSupplied }` | `AuthenticationRequired { message, credentials_were_supplied }` | no |
| `ProxyAuthenticationException` | `ProxyAuthentication(String)` | no |
| `InsufficientDiskSpaceException(SpaceShortfall)` | `InsufficientDiskSpace(SpaceShortfall)` | no |
| `RemoteFileChangedException` | `RemoteFileChanged(String)` | no |
| `StreamNotSupportedException` | `StreamNotSupported(String)` | no |
| `FtpRestartNotSupportedException` | `FtpRestartNotSupported` | no |
| `IOException` / `HttpRequestException` / timeouts | `Io`, `Http { status, message }`, `Timeout`, `Cancelled` | yes (except `Cancelled`) |

The user-facing message strings are copied verbatim from C#, so status bars,
dialogs and `qb` print the same text.

### 4.5 Design patterns: kept and improved

| Pattern | C# | Rust |
|---|---|---|
| Factory | `IConnectionFactory` / `ProtocolConnectionFactory` | `trait ConnectionFactory`, with `ProtocolConnectionFactory` dispatching on the scheme |
| Strategy | `RetryPolicy` | `RetryPolicy` struct with a pluggable `is_retryable` classifier |
| Facade / Registry | `DownloadManager` | Same |
| Observer | events + `IDispatcher` | `EventHub` + `Dispatcher` (§4.2) |
| Repository | `DownloadRepository`, `QueueRepository` | Same, with atomic write-then-rename saves |
| Composition root | `Program.cs` | `quickbyte-ui/src/main.rs`: the only place that knows concrete types |

**Improvements over the C# code:**

- **Atomic persistence.** Every JSON save writes `*.json.tmp`, flushes it and
  renames it over the original (`MoveFileExW` with `REPLACE_EXISTING |
  WRITE_THROUGH`). A crash mid-write can no longer truncate `downloads.json`.
  The agent's "a half-written `queues.json` reads as *try again*, not as *no
  queues*" rule is kept.
- **Debounced saves.** Progress no longer causes a write of the whole list.
  Saves are coalesced on a 1 s debounce, plus an immediate save on every status
  change and on exit.
- **Unknown-field preservation** in every persisted model (§5.6), so a later
  or earlier build's settings survive a round trip.
- **No per-tick allocation** in the progress path. Snapshots reuse a `Vec`, and
  the UI repaints only while something is animating.
- **Typed errors** in place of exception filters (§4.4).

---

## 5. Data compatibility (must not change)

Everything in this section is a hard compatibility contract. Each item has a
round-trip test, built from fixtures captured from a real 1.26.2 profile.

### 5.1 Locations

| What | Path |
|---|---|
| Data folder | `%APPDATA%\QuickByte` (`Environment.SpecialFolder.ApplicationData`) |
| Settings / downloads / queues | `settings.json`, `downloads.json`, `queues.json` |
| Chunk folder | `%APPDATA%\QuickByte\temp\<guid N format>` |
| Per-download state files | `segments.json`, `merge.json`, `stream.json` inside the chunk folder, same schemas |
| Agent log | `%APPDATA%\QuickByte\agent.log` (same line format, same size cap) |
| Updater scratch | `%TEMP%\QuickByte\updates` |
| Install folder (per-user Inno default) | `%LOCALAPPDATA%\Programs\QuickByte` |

### 5.2 JSON encoding rules (System.Text.Json defaults)

- **Property names are PascalCase** in the three profile files, exactly as the
  C# property names. The command-line `--json` output and pipe responses are
  camelCase, and the update manifest uses its explicit lower-camel names.
- **Reading is case-insensitive** where the C# options said so (pipe, browser
  bridge, update manifest). Profile files are read with the exact names, with
  a case-insensitive fallback for safety.
- **Enums are numbers**: `Status: 5`, `Mode: 0`, `Days: 127`, …
  `serde_repr` keeps the discriminants identical to C#.
  `DownloadStatus::Verifying = 8` stays last.
- **Dictionary keys that are enums are names**:
  `CategoryFolders: { "Video": "D:\\Films" }`.
- **`Guid`**: lower-case, hyphenated (`"d8d7a993-496a-4bfa-85e5-2d9e20ecf2eb"`).
  The chunk folder name uses the `N` format (no hyphens).
- **`DateTimeOffset`**: ISO 8601 with up to 7 fractional digits and an offset,
  for example `"2026-11-09T10:08:25.1128086+00:00"`. It is written back with 7
  digits and the original offset.
- **`TimeSpan`**: .NET's constant format `[-][d.]hh:mm:ss[.fffffff]`, for example
  `"02:00:00"`, and `"1.02:03:04.5"` for spans over a day. Implemented in
  `core::serde_compat::timespan`.
- **Computed properties are still written** (`FullPath`, `AudioFullPath`,
  `ProgressPercentage`, `Category` on `DownloadItem`), so a file written by the
  Rust build still reads in the C# build if someone rolls back. They are
  ignored on read.
- **A UTF-8 BOM** at the start of a file is accepted on read. Files are written
  without one, as System.Text.Json did.
- **Indented output** (`WriteIndented = true`), two-space indentation, so files
  stay human-diffable.

### 5.3 The three profile files

The field lists are exactly those of `DownloadSettings` (+ `ProxySettings`),
`DownloadItem` (+ `DownloadCredentials`, `RemoteFileVersion`, `StreamSource`)
and `DownloadQueue` (+ `QueueSchedule`) in the C# `Models/` folder. Defaults
match the C# property initialisers, so a missing key reads as it did before.
`[JsonIgnore]` members (`Password`, `TempFolder`, `IsComplete`, …) are
`#[serde(skip)]`.

### 5.4 Secrets (DPAPI)

`SecretProtector` is ported exactly: `CryptProtectData` with **entropy =
UTF-8 bytes of `"QuickByte.Credential.v1"`**, `CurrentUser` scope (no
`CRYPTPROTECT_LOCAL_MACHINE`), and base64 encoding. An empty password
serialises as `null`. A failure to decrypt yields an empty string, never an
error. So `ProtectedPassword` values written by the C# build decrypt in Rust,
and the reverse.

### 5.5 Names shared with Windows and other processes

| Name | Value | Used by |
|---|---|---|
| App mutex | `Local\QuickByte.SingleInstance` | app (and the agent checks it) |
| Agent mutex | `Local\QuickByte.QueueAgent` | agent (and the app checks it) |
| Hand-off pipe | `\\.\pipe\QuickByte.SingleInstance.<UserName>` | second launch → running app. Payload: a URL, or `run-queue:<guid>`. Write-then-close, no reply. |
| Command pipe | `\\.\pipe\QuickByte.Command.<UserName>` | `qb` ↔ app. Newline-delimited JSON, one request and one reply, 64 Ki-char cap, current-user-only. |
| Run value (app) | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\QuickByte` = `"<path>\QuickByte.exe"` | `StartupRegistration` |
| Run value (agent) | `…\Run\QuickByteScheduler` = `"<path>\QuickByte.Agent.exe"` | `QueueAgentRegistration` |
| Wake task | `QuickByte Queue Wake (<UserName>)`, same XML definition | `QueueWakeRegistration` |
| Browser bridge | `127.0.0.1:<BrowserIntegrationPort>` (9614), `GET /ping`, `POST /download`, header `X-QuickByte-Token`, CORS only for `chrome-extension://` / `moz-extension://` | `BrowserIntegrationServer` |
| User-Agent | `QuickByte/1.0 (+https://github.com/ismailian/quickbyte)` unless customised | every request |
| Update manifest | `https://quickbyte-app.pages.dev/releases/latest.json` | `UpdateService` |
| Command-line switches | `--minimized` (`-minimized`, `/minimized`, `-m`), `--run-queue <guid>`, `run-queue:<guid>`, a bare URL | app; agent `--once` |

Because the Rust build reuses these names, an old C# process and a new Rust
process can never both run: whichever starts second sees the mutex and hands
off. During the upgrade, the installer closes the old one first anyway (§6).

### 5.6 Forward and backward compatibility

Every persisted struct has

```rust
#[serde(flatten)]
pub extra: serde_json::Map<String, serde_json::Value>,
```

so keys this build does not know (the 1.26.2 VirusTotal settings, a download's
`VirusTotal` object) are read, carried along and written back unchanged.

### 5.7 In-flight downloads

A download that was paused, or interrupted mid-transfer, under the C# build
resumes under the Rust build from the same chunks. The chunk file naming,
`segments.json` (the dynamic-segmentation layout), `merge.json` (the interrupted
merge plan) and `stream.json` (the HLS/DASH piece state) keep their schemas and
are read by the same rules. There is an integration test that builds a chunk
folder by hand in the C# layout and resumes it.

---

## 6. Installer and seamless upgrade

The C# release was built with Inno Setup 6. The installed copy shows:

```
AppId           {E15CA2A4-BDCB-49FB-BEE6-810D219CA5BD}   (uninstall key …\Uninstall\{E15CA2A4-…}_is1, HKCU)
Install mode    per-user (PrivilegesRequired=lowest) → %LOCALAPPDATA%\Programs\QuickByte
Publisher       Cyberliberty, Inc.
URLs            https://quickbyte-app.pages.dev/
Tasks           desktopicon, addtopath  (+ browserintegration once a store ID exists)
```

The new `rust/installer/QuickByte.iss` makes the switch invisible:

1. **The same `AppId`, per-user, with `UsePreviousAppDir=yes`.** Inno treats
   the new setup as an upgrade of the same app. It installs into the same
   folder, reuses the same uninstaller entry (`unins000.exe` and `.dat` are
   appended to, not duplicated), and remembers the tasks the user picked.
   There is no second entry in *Apps & features*.
2. **Close everything that holds a file.** `CloseApplications=force` with
   `CloseApplicationsFilter=*.exe,*.dll` uses the Restart Manager. Before that,
   a `[Code] PrepareToInstall` step ends `QuickByte.exe`,
   `QuickByte.Agent.exe` and `qb.exe` **when they run from the install
   folder**. The agent has no window, so Restart Manager cannot always ask it
   to close. The old app is closing itself anyway when the update came from the
   in-app updater.
3. **Remove the .NET build's leftovers** with `[InstallDelete]`: `*.dll`
   (`QuickByte.dll`, `QuickByte.Core.dll`, `QuickByte.Agent.dll`, `qb.dll`,
   `System.Security.Cryptography.ProtectedData.dll`), `*.runtimeconfig.json`,
   `*.deps.json` and `*.pdb`. Only the three native executables are left, plus
   the uninstaller and the `extensions` folder.
4. **No .NET runtime prerequisite.** The C# installer's check for the .NET 8
   Desktop Runtime is gone. The Rust executables link the CRT statically
   (`-C target-feature=+crt-static`), so they need nothing installed.
5. **Keep the user's data.** Nothing under `%APPDATA%\QuickByte` is touched on
   install or upgrade. Uninstall leaves it in place too, as before.
6. **Re-assert registrations after install.** On first launch the app runs
   `StartupRegistration::sync`, `QueueAgentRegistration::sync` and
   `QueueWakeRegistration::sync`, as the C# app did. The Run values and the
   wake task already point at the same paths, so they are rewritten only if
   they differ.
7. **Uninstall** deletes the `QuickByteScheduler` Run value and the wake task,
   as the C# installer did. `QuickByte` (Start with Windows) is also deleted.
8. **Launch after install** (`[Run] … Flags: nowait postinstall
   skipifsilent`), unchecked when the installer was started by the updater
   with `/SILENT`. In that case the installer restarts QuickByte itself with
   `--minimized` when the user had it running.
9. **The old updater can install it.** The C# `UpdateForm` downloads
   `downloadUrl` from `latest.json`, verifies its SHA-256 and runs it with
   `UseShellExecute`. The Rust installer is an ordinary Inno setup `.exe`, so
   the C# 1.26.x updater installs 1.27.0 with no changes on the C# side.

**Publishing the release** (`scripts/build-release.ps1`):

```
cargo build --release --workspace
stage target/release/{QuickByte.exe, quickbyte-agent.exe → QuickByte.Agent.exe, qb.exe}
ISCC.exe installer\QuickByte.iss  → installer\Output\QuickByte-1.27.0-Setup.exe
write site/releases/latest.json   { version, downloadUrl, releaseNotes, releaseDate, fileSizeBytes, sha256 }
```

---

## 7. UI specification (egui)

### 7.1 Framework choices

- **eframe 0.37 with the `glow` (OpenGL) renderer.** It is a much smaller
  binary and uses less memory than `wgpu`, and is ample for a 2-D tool UI.
  Persistence of window geometry uses eframe's `persistence` feature, in
  `%APPDATA%\QuickByte\ui.ron`.
- **Multiple native windows** through egui **deferred viewports**. Details,
  Add Download, Complete and Update are each a separate OS window with their
  own taskbar button, unowned and modeless, as in WinForms. "Close to tray"
  hides all of them and restores the same set.
- **Repaint only when needed.** The UI calls `request_repaint_after(16 ms)` only
  while a progress animation is running or a download is active. Otherwise it
  sleeps until an event or input wakes it. A minimised window or a hidden tray
  state does no rendering at all.

### 7.2 Compact type and controls (the user's requirement)

| Token | Value |
|---|---|
| Body / Button / Monospace text | **12 px** (`egui::FontId::proportional(12.0)`) |
| Small text (status bar, hints) | 11 px |
| Heading (dialog titles inside a window) | 13 px, strong |
| Button padding | 6 × 2 px |
| Interact height (buttons, combo boxes, text fields) | 20 px |
| Item spacing | 6 × 4 px |
| Toolbar icon buttons | 24 × 24 px with 16 px icons, and a tooltip naming the shortcut |
| Table row height | 20 px |
| Corner radius | 3 px; window stroke 1 px `Border` |

The font is Segoe UI, loaded from `%WINDIR%\Fonts\segoeui.ttf` at startup, with
Segoe UI Symbol (`seguisym.ttf`: ✓ ▲ ▼) and Segoe UI Emoji (`seguiemj.ttf`: ⏱)
behind it and egui's bundled font last, so the app looks native. Nothing is
bundled, so the binary does not grow.

### 7.3 Palette

`theme.rs` carries both palettes from `Theme.cs` value for value (Window,
Surface, SurfaceAlt, Chrome, Sidebar, HeaderBack, Track, Border, BorderStrong,
Text, TextMuted, TextDisabled, TextOnAccent, Accent, AccentDark, AccentLight,
AccentSoft, AccentPressed, Success, SuccessLight, Warning, Danger, RowSelected,
RowSelectedInactive, RowAlt). `AppTheme::System` follows
`HKCU\…\Themes\Personalize\AppsUseLightTheme`. One improvement over WinForms:
egui restyles every frame, so **a theme change applies immediately**, with no
restart needed. The WCAG contrast tests in `ThemeTests.cs` are ported.

Every window also gets a dark title bar in dark mode (`DwmSetWindowAttribute
DWMWA_USE_IMMERSIVE_DARK_MODE`).

### 7.4 Windows (one module each)

| C# form | Rust module | Contents |
|---|---|---|
| `MainForm` (2 077 lines) | `windows/main.rs` | Menu bar (File / Tasks / Downloads / View / Help), icon toolbar, search box (Ctrl+F), category tree sidebar with a Queues branch, a virtualised downloads table (`egui_extras::TableBuilder`, sortable, multi-select, owner-drawn progress cells, per-type icons), row context menu, status bar with counts and total speed, tray hand-offs, keyboard shortcuts (Ctrl+N, Del, Enter, Ctrl+A, …) |
| `DownloadDetailsForm` | `windows/details.rs` | Three tabs (Download status / Speed Limiter / Options on completion), the header, the field grid, an animated progress bar, the collapsible details section, the connection segments bar, the connections table with a per-connection context menu, and taskbar progress |
| `DownloadCompleteForm` | `windows/complete.rs` | Final size, average speed, folder, Open / Open Folder / Verify, and "don't show again" |
| `NewDownloadForm` | `windows/new_download.rs` | URL (pre-filled from the clipboard), probe in flight, name, Save to (`~\…` shortening), category-folder following, connections, Queue, Quality row for streams, Checksum, and the sign-in hand-off |
| `BatchDownloadForm` | `windows/batch.rs` | Paste / import / pattern, probing six at a time, a tick list, queue target |
| `SettingsForm` | `windows/settings.rs` | Tabs: Connection / Folders / Interface / System / Browser / Proxy. Live-applied settings take effect on Save. |
| `QueueManagerForm` | `windows/queues.rs` | A queue list, plus Files / Options / Schedule tabs that apply as they are edited, a next-run line, and agent availability |
| `UpdateForm` | `windows/update.rs` | Installed vs offered version, notes, a progress bar, and the silent (startup) and manual paths |
| `CredentialsForm` | `windows/credentials.rs` | A modal sign-in that names the host and repeats the server's message |
| `ChecksumForm` | `windows/checksum.rs` | Verify a finished file |
| `RefreshAddressForm` | `windows/refresh_address.rs` | Paste or re-capture, judge, update / start over |
| `PowerActionCountdownForm` | `windows/power_countdown.rs` | A countdown with Cancel as the default, and the count of other running downloads |
| `AboutForm` | `windows/about.rs` | Version and copyright |

Progress smoothing (`ProgressAnimator<TKey>`) is ported as a generic
`Animator<K>`. It eases toward each sample at the display frame rate, never
steps backwards, and holds the bar at 100% with overlay text while a download
merges.

---

## 8. Performance plan

| Area | C# | Rust |
|---|---|---|
| Startup | JIT + WinForms init (~400–700 ms cold) | **Measured:** window on screen 528 ms after launch (release, cold, including the OpenGL context) |
| Idle memory | ~60–90 MB (CLR + WinForms) | **Measured:** ~83 MB working set, ~42 MB private, mostly the OpenGL driver context. The earlier target of under 30 MB is not met with the glow renderer. |
| Idle CPU | WinForms timers tick | **Measured:** ~0.5% of one core (≤47 ms per 10 s, main window shown or in the tray). The first few launches of a freshly built exe showed a busy *unnamed* thread, consistent with the GPU driver building its shader cache; re-check on each release. |
| Install size | ~1.6 MB of assemblies, plus a ~55 MB .NET Desktop Runtime prerequisite | **Measured:** QuickByte.exe 14.4 MB, QuickByte.Agent.exe 0.7 MB, qb.exe 1.0 MB; the installer is 6.2 MB; no prerequisite |
| Hot loop | `Stream.ReadAsync` → `FileStream.WriteAsync`, `Interlocked` | One reused buffer per connection, `AtomicU64`, write-through `BufWriter` sized to the read buffer |
| Agent | .NET runtime per wake (~30 MB) | ~2 MB resident; sleeps on a single timer |
| `qb.exe` | .NET start per call (~100 ms) | ~5 ms start |

Release profile: `opt-level = 3`, `lto = "fat"`, `codegen-units = 1`,
`strip = true`, `panic = "abort"` for the agent and CLI only (the GUI keeps
unwinding so a panicking window can be caught and reported), and
`+crt-static`.

---

## 9. Testing strategy

- **Every C# test is ported.** That covers 50+ Core test classes (helpers,
  models, services with fakes, streaming, proxy, persistence), the agent's
  scheduler tests, and the UI project's non-visual tests (command-line
  parsing, registrations, icon writer, animator, theme contrast). Each lands
  next to the code it covers.
- **Compatibility fixtures**: real `settings.json`, `downloads.json` and
  `queues.json` from a 1.26.2 profile (with URLs and secrets scrubbed) must
  load, then save byte-equivalently, ignoring whitespace.
- **Local test servers**: a tiny hyper server in `tests/` serves ranges, `200`
  on a cache hit, `If-Range` mismatches, 401 Basic/Digest, 407 and 429, as the
  C# `StubWeb` did, so the pool, recovery and version checks are exercised end
  to end.
- `cargo clippy --workspace -- -D warnings` and `cargo test --workspace` must
  pass before each milestone is ticked.

---

## 10. Risks and how they are handled

| Risk | Mitigation |
|---|---|
| NTLM/Negotiate have no mature pure-Rust client | SSPI through the `windows` crate. The handshake runs on a dedicated single-connection HTTP/1.1 client per origin and login, which is the same shape as C#'s `AuthenticatedHttp`. |
| System proxy: .NET honoured WinINET settings, including PAC | `WinHttpGetIEProxyConfigForCurrentUser` gives the static proxy and bypass list, and PAC / WPAD go through `WinHttpGetProxyForUrl` when a script or auto-detect is configured. Core still has no registry code. NTLM proxy auth uses SSPI with the signed-in user's credentials. |
| NTLM / Negotiate login **to a proxy** in System mode (C# sent the signed-in user's credentials) | **Open gap.** reqwest cannot run that handshake with a proxy, so such a 407 is reported as a proxy error and not retried. Planned fix: an SSPI handshake over `ProxyTunnel` for proxied requests. A proxy named in Options with Basic auth works. |
| A redirect to another host drops a captured `Cookie` header (reqwest), where C# kept it | Accepted for now. It only matters for a cross-host redirect that needs the page's cookies. Revisit if a capture reports it. |
| egui multi-window focus and taskbar behaviour | Deferred viewports, each with its own HWND (taskbar progress and dark title bars attach to it). Tested manually on Windows 10 and 11. |
| A JSON drift corrupting a profile | Fixture round-trip tests (§9), atomic saves (§4.5), and unknown-field preservation (§5.6) |
| A user downgrades to the C# build | Files stay readable by the C# build (computed fields are still written, enums are unchanged) |

---

## 11. Milestones

Each box is ticked when the work is merged, builds in release, and its tests
pass.

### M0: Plan and scaffold
- [x] Read the C# solution, its README and the installed footprint
- [x] Write this migration document
- [x] Cargo workspace, five crates, release profile, version single-sourced
- [x] Core contracts: enums, models (with serde compatibility), events, errors, traits

### M1: Core foundations (pure logic)
- [x] serde compatibility layer (TimeSpan, DateTimeOffset, enum-keyed maps, BOM, extras)
- [x] Helpers: ByteFormatter, RangeSplitter, SpeedCalculator, RetryPolicy, FileNameHelper, FileCategoryHelper, ProductVersion, UrlCredentials, UserPath, ClipboardLinks, BatchLinks, DownloadSearch, FileChecksum, FileVersionCheck, AddressRefresh, CompletionPlan, ConnectionActions, DownloadActions, WakeTimes, SettingsKeys, AppUserAgent, ProxyBypass, DiskSpace, MarkOfTheWeb, SecretProtector (DPAPI), BandwidthLimiter (+ composite)
  - [x] UrlCredentials, UserPath, ClipboardLinks, BatchLinks, DownloadSearch, FileChecksum, CompletionPlan, ConnectionActions, DownloadActions, WakeTimes, SettingsKeys, DiskSpace (+ `platform::disk`), MarkOfTheWeb (+ `platform::alternate_stream`), BandwidthLimiter (+ composite, unlimited)
  - [x] RangeSplitter, SpeedCalculator, RetryPolicy, FileNameHelper, ProductVersion, FileVersionCheck, AppUserAgent, ProxyBypass (`net::proxy_bypass`) — with M2
  - [x] AddressRefresh — with M3
- [x] QueueSchedule / DownloadQueue scheduling arithmetic
- [x] Streaming parsers: HLS, DASH, StreamPlan, StreamDetector, StreamState
  - [x] HLS, DASH, StreamPlan, StreamState
  - [x] StreamDetector — with M2
- [x] Commands: CommandLine grammar, messages, formatter
- [x] Repositories + SettingsService with atomic saves (fixture round-trip tests for the three profile files: done)
  - [x] DownloadRepository, SettingsService (by-name get/save included)

### M2: Core networking
- [x] Shared HTTP client, AppProxy (system / none / manual HTTP·SOCKS5·SOCKS4), ProxyTunnel
- [x] HttpRequestDecorator, Basic pre-emptive, Digest, NTLM/Negotiate (SSPI), the 407 path
- [x] RemoteFileInfoProvider (HEAD + ranged probe, cache-bypass retry, validators)
- [x] DownloadConnection (Range + If-Range, reserve-before-read, shrinkable end)
- [x] FTP/FTPS: control channel, PASV/EPSV, REST STREAM, info provider, connection
- [x] Protocol dispatch (info provider + connection factory)
- [x] StreamResolver + playlist fetch, UpdateService, BrowserIntegrationServer

### M3: Core engine
- [x] SegmentMap / SegmentRange, ConnectionPoolManager (dynamic segmentation, recovery, per-connection control, throttled progress)
- [x] FileMerger (resumable merge with merge.json, delete as it goes)
- [x] DownloadService (lifecycle, disk check, version check, MOTW, checksum)
- [x] StreamDownloadService (pieces, AES-128, resume by piece, two-track output)
- [x] DownloadManager (registry, semaphore, persistence, name reservation, three-tier limits)
- [x] QueueManager (runner loop, schedule timer, drained detection)
- [x] CommandProcessor + the current-user-only command pipe

### M4: Shell integration (quickbyte-shell)
- [x] SingleInstance (mutex + hand-off pipe), AppVersion
- [x] StartupRegistration, QueueAgentRegistration, QueueWakeRegistration (schtasks XML)
- [x] PowerActions, KeepAwake, ZoneInformationPolicy, FileLauncher
- [x] Tray icon + balloons, clipboard watcher, taskbar progress, dark title bars

### M5: Agent and CLI
- [x] quickbyte-agent: SchedulerLoop, AgentLog, `--once`, mutex, self-exit
- [x] quickbyte-cli: `qb` with exit codes 0/1/2/3, `--json`, and starting the app when needed

### M6: UI (egui)
- [x] Theme (both palettes, 12 px scale, compact spacing), icons, widgets, animator
- [x] Main window (menu, toolbar, sidebar, table, context menu, search, status bar, tray)
- [x] Download details + complete windows (taskbar progress)
- [x] App architecture: shared view model + event pump, `UiDispatcher`, deferred-viewport window registry (`windows::AppWindow` / `WindowKind` / `DialogRequest` → `build_dialog`), close-to-tray of the whole set, repaint-only-when-needed, `--demo` services
- [x] Add download (probe, stream quality, credentials hand-off) + batch download
- [x] Settings (all tabs), Queues & Scheduler, Update, About, Checksum, Refresh address, Power countdown (helpers still being ported — `file_name_helper`, `app_user_agent`, `stream_detector`, `address_refresh`, `file_version_check` — have marked `// INTEGRATION:` copies in `quickbyte-ui/src/actions.rs`)
- [x] Composition root: settings, proxy and User-Agent wiring, hand-offs, browser captures, command pipe, startup queue and URL (the `Program.cs` order is in `main.rs`, with demo services behind `build_services` and every real service marked `// INTEGRATION:`)

### M7: Packaging and upgrade
- [x] build.rs VERSIONINFO + icon for all three executables
- [x] Inno script with the same AppId, the close-and-clean upgrade, and uninstall cleanup
- [x] build-release.ps1 → Setup.exe + latest.json
- [ ] Upgrade rehearsal: install over 1.26.2, then check that data, the token, the Run values, the wake task and a paused download all carry over

### M8: Hardening
- [x] `cargo clippy -D warnings`, the full test suite, a release build
- [x] Manual smoke test: real downloads (HTTP, FTP, HLS), pause/resume across restarts, queues, `qb`, browser capture
  - [x] HTTP over 8 connections through `qb` (hash identical to curl), Mark of the Web written, completion window
  - [x] Pause → force-kill → relaunch → resume: every connection continued from its chunk length; merged file byte-identical
  - [x] FTP (ftp.gnu.org, 4 connections, hash identical), HLS (local playlist, pieces joined byte-identical, generic name replaced by the folder name)
  - [x] Scheduler agent `--once`: defers to a running app; with the app closed launches `--run-queue <id> --minimized`, LastRunAt persisted
  - [x] Browser bridge: token required (401 without), no CORS for web origins, a capture opens Add Download already probed
- [x] Update the C# README's pointer, and write `rust/README.md` with build instructions

### M9: Catch up with the unpushed 1.26.x C# changes
- [ ] Pull the newer C# source and diff `src/` against `c32b573`
- [ ] Port each change (VirusTotal and anything else) into its Rust module, with its tests
- [ ] Re-run the compatibility fixtures against a 1.26.2 profile
