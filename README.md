# Telegram Downloader RS

🇬🇧 English · 🇻🇳 [Tiếng Việt](README.vi.md)

Resumable, persistent background downloads of very large Telegram files for
Windows x64 — for example multi-part archives like `CHINT.7z.001…026`.

- Rust core (MTProto via Grammers, SQLite via `winsqlite3.dll`, no C toolchain needed)
- `tgdlrs-worker` background process, independent of OpenCode; auto-recovers interrupted jobs
- `tgdlrs` CLI, a bilingual login GUI and a bundled MCP server (`telegram-downloader`)
- True byte-level resume against Telegram's `upload.getFile` (512 KiB aligned chunks)
- Docs: [English guide](docs/USER-GUIDE.en.md) · [Tiếng Việt](docs/USER-GUIDE.vi.md)
- Plan: [English](docs/PROJECT-PLAN.en.md) · [Tiếng Việt](docs/PROJECT-PLAN.vi.md)

## Binaries

| Binary | Purpose |
| --- | --- |
| `tgdlrs.exe` | CLI; also runs the MCP server with `tgdlrs mcp` |
| `tgdlrs-worker.exe` | Background worker; owns the Telegram session |
| `tgdlrs-mcp.exe` | MCP server (same code as `tgdlrs mcp`) |

Installed under `%LOCALAPPDATA%\tgdlrs\bin\`; self-contained (system DLLs only;
SQLite is loaded from `winsqlite3.dll` at runtime).

## First-time setup

1. Get an `api_id`/`api_hash` pair from <https://my.telegram.org> (*API development tools*).
2. Either run the bilingual login GUI — install with:

   ```bat
   powershell -NoProfile -ExecutionPolicy Bypass -File extras\install-login-gui.ps1
   ```

   then double-click **Telegram Downloader - Login** on your Desktop — or put the
   credentials directly into `%APPDATA%\tgdlrs\config.json` (created on first run) or
   set `TGDLRS_API_ID` / `TGDLRS_API_HASH`.
3. Log in once. In the GUI you are asked for your phone number, the OTP sent by
   Telegram and your 2FA password when applicable. The same values also work from
   `tgdlrs login` in a terminal. **Never paste these into chat.**
4. `tgdlrs daemon install-autostart` to run the worker at Windows login.

## Usage

```powershell
tgdlrs download "https://t.me/c/1188194766/1914"   # enqueue; worker does the rest
tgdlrs status                                       # worker + queue summary
tgdlrs list [--state RETRY_WAIT] [--limit 20]
tgdlrs pause <id> | resume <id> | retry <id> | cancel <id>
tgdlrs verify <id|path>                             # size + SHA-256
tgdlrs scan -1001188194766                          # list documents in a chat
tgdlrs doctor                                       # diagnostics
tgdlrs daemon start|stop|status|install-autostart
tgdlrs --json <command>                             # machine-readable output
```

`<id>` may be a full id or any unique prefix. `pause` is observed by the worker
within seconds; `.part` files are never deleted, never concatenated across
sources, and only renamed to the final path after size + SHA-256 verification.
On restart, an unaligned tail of a partial file is truncated back to the last
512 KiB chunk boundary and re-downloaded.

## MCP

`tgdlrs mcp` speaks MCP over stdio. The OpenCode server is registered in
`opencode.json` under `mcp.servers["telegram-downloader"]`. It exposes the
`telegram_*` tools (see `docs/USER-GUIDE.en.md`, section 7) and never blocks on a
whole download; it only enqueues/controls jobs. Errors are JSON objects with
`kind` and `message` fields (e.g. `flood_wait`).

## The login GUI (`extras/`)

- `tgdlrs-login.ps1` / `tgdlrs-login.cmd` — bilingual (EN / Tiếng Việt) login
  window, installable with `extras/install-login-gui.ps1`.
- Sections: login status & credentials, **download folder** (edit/browse, applies
  to new jobs) and **Save folder**, plus Quick help in both languages.
- `tgdlrs-notify.ps1` — small toast window (bottom-right, auto-close, `gui_lang`
  aware) shown whenever a download **finishes or fails**.
- OTP/2FA stay hidden in the console; only `api_id`/`api_hash`/phone are stored,
  in `%APPDATA%\tgdlrs\config.json` (timestamped backup is kept on save).

## Resume semantics (honest)

- Only complete 512 KiB-aligned prefixes of a `.part` file are trusted. Bytes in
  the unaligned tail are re-downloaded; `redownloaded_bytes` reports the cost.
- If Telegram answers with a CDN redirect for a file, Grammers panics on our
  build; it is caught and surfaced as a clear, non-retryable `download` error
  instead of taking the worker down. No CDN support is claimed.
- Retry uses exponential backoff with jitter; `FLOOD_WAIT` is honored. Network
  errors are retried up to `max_attempts`; non-retryable errors fail the job.
- `SKIP_VERIFIED` never overwrites an existing final file; a mismatch is a hard
  conflict error.

## Build & test

```powershell
cargo build --release --workspace
cargo test --workspace     # set TMP/TEMP to a writable folder (e.g. target\tmp)
cargo clippy --workspace --all-targets -- -D warnings
```

The workspace uses `stable-x86_64-pc-windows-gnu` (see `rust-toolchain.toml`).
The GNU toolchain needs a working `as`/`dlltool` on `PATH`; on this machine the
MinGW-w64 distribution at `E:\VScode_Opencode\Tools\mingw64\mingw64\bin` is used.
`cargo test` needs a writable temp dir; point `TMP`/`TEMP` at `target\tmp` if the
system temp dir is not writable in your environment.

## Repository layout

- `crates/tgdlrs-core` – FFI SQLite, models, resumable engine, IPC, worker logic
- `crates/tgdlrs-cli` – `tgdlrs` binary
- `crates/tgdlrs-worker` – background worker binary
- `crates/tgdlrs-mcp` – MCP server (library + `tgdlrs-mcp` binary)
- `extras/` – bilingual login GUI + installer
- `docs/` – guides and project plan (EN/VI)

Default download directory: `D:\Downloads\Brave` (configurable in
`%APPDATA%\tgdlrs\config.json`). Existing files are never overwritten without a
verified size/identity match; `.part` files are kept on failure.

## License

MIT
