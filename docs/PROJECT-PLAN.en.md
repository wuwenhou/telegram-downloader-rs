# Telegram Downloader RS — Project plan (English)

This document describes the **design decisions, architecture and current
status** for maintainers and agents (OpenCode). State is recorded as of
**2026-11-14**.

---

## 1. Goals

1. Download very large Telegram files (`CHINT.7z.001…026`, ~4.17 GB each,
   ~108.6 GB in total) into `D:\Downloads\Brave` without re-downloading parts
   that are already complete (`.001`–`.020` and `.023`, all exactly
   4,175,429,632 bytes).
2. **Real byte-level resume** after: network loss, process kill, Windows restart.
3. Enqueue/control from CLI and from OpenCode (MCP), without blocking the chat.
4. Worker runs detached from OpenCode, auto-recovers interrupted jobs, starts at
   Windows logon, no Administrator rights.
5. SHA-256 verifies every part; the final file is renamed only after size matches.
6. Never: overwrite an existing file before identity is confirmed, concatenate
   bytes from different sources, delete `.part` files.

## 2. Requirements & constraints

| # | Requirement | Decision |
|---|---|---|
| R1 | Core engine in Rust, no C toolchain | `winsqlite3.dll` FFI via `libloading`; Grammers (pure Rust) |
| R2 | Windows x64 | `rust-toolchain.toml` → `stable-x86_64-pc-windows-gnu` |
| R3 | Byte-level resume for Telegram | `upload.getFile`, 512 KiB chunks, `skip_chunks`; unaligned tail truncated |
| R4 | Session survives restart | `grammers-session` + custom `FileSession` (atomic JSON persist) |
| R5 | Queue + pause/resume/retry/cancel + FloodWait | Worker polls SQLite every ~2 s; `DownloadControl` set from DB state |
| R6 | No TCP ports | Named pipe `\\.\pipe\tgdlrs-<user>` + DACL user+SYSTEM; JSON-lines |
| R7 | CLI commands login/download/scan/status/list/pause/resume/retry/verify/doctor/daemon/mcp | Implemented, `--json` on most commands |
| R8 | MCP 15 tools, non-blocking, structured I/O | `rmcp` tool_router; enqueue/controls only |
| R9 | No secrets in chat/log/git | Session + creds live only in `%APPDATA%\tgdlrs`; `.gitignore` |

Environment constraints: no MSVC linker / Windows SDK / CMake / libsqlite3-C, so
we use the GNU toolchain with the local MinGW-w64 (`as`/`dlltool`); `cargo test`
needs `TMP`/`TEMP` pointing at a writable folder (`target\tmp`).

## 3. Architecture

```
┌──────────────┐   DB writes (pause/resume/…)   ┌────────────────┐
│ tgdlrs CLI   │────────────────────────────────▶│                │
└──────┬───────┘                                 │  state.db      │
       │ IPC (named pipe, JSON-lines)            │  (winsqlite3,  │
       ▼                                         │   WAL)         │
┌──────────────┐   IPC (scan/resolve/search)    │                │
│ tgdlrs-mcp   │◀───────────────────────────────│                │
│ (stdio)      │                                └──────┬─────────┘
└──────┬───────┘                                       │ poll ~2s
       │ enqueue (SQLite)                              ▼
┌──────┴───────┐                             ┌──────────────────┐
│ tgdlrs-worker│◀────────────────────────────│   (queue/control) │
│  (detached)  │   Telegram MTProto          └──────────────────┘
└──────────────┘
```

- The **worker** is the only process holding the Telegram session. CLI/MCP
  record intent in SQLite; the worker notices within ~2 s (pause/resume/cancel).
- The **MCP** server talks to Telegram only through the worker; long operations
  (scan/search) have a client-side timeout. MCP never downloads files.
- CLI `login` stops the worker, runs a temporary client, then restarts the worker.

## 4. Data model & states

`%APPDATA%\tgdlrs\state.db` — tables: `downloads`, `download_parts`,
`download_events`, `telegram_sources`, `verification_results`, `settings`,
`schema_migrations`.

Lifecycle: `QUEUED → RESOLVING → DOWNLOADING → VERIFYING → COMPLETED → VERIFIED`
(error branch: `RETRY_WAIT → QUEUED` or `FAILED`; `PAUSED` from most states;
`CANCELED` from allowed states; re-verify can move a job to `FAILED`).

## 5. Resume design (the core)

1. Every job uses `<safe_name>.part` (or `<safe_name>.<id8>.part` on collision).
2. `plan_resume(part_len, expected_size)`:
   - empty → start at 0;
   - chunk-aligned → continue at offset;
   - unaligned → truncate `.part` back to the last 512 KiB boundary, count `redownloaded_bytes`;
   - `.part` larger than expected → restart from 0 (bytes kept for inspection).
3. Source: `upload.getFile` requires 4 KiB-aligned offsets; Grammers
   `iter_download().chunk_size(512 KiB).skip_chunks(offset / 512 KiB)`;
   unaligned offset handled via `pending_skip` (fetch the aligned chunk, drop the prefix).
4. After a network error the `DownloadIter` is recreated at the current written
   offset (Grammers iterators die after an error); exponential backoff with
   jitter, capped at 60 s; FloodWait is honored by sleeping exactly as required.
5. Completion: if `final_path` exists → verify; match → atomic no-overwrite
   rename; mismatch → job FAILED, existing file untouched; corrupt `.part`
   quarantined to `<part>.bad-<timestamp>` after a failed post-download hash check.

### Known limitations (honest)

- If Telegram answers with a **CDN redirect**, Grammers (built with
  `cdn_supported = false`) panics; we catch the panic and surface a clear,
  non-retryable `download` error — CDN is not supported.
- Long FloodWaits are honored (account safety over speed).
- `skip_verified` skips re-download, not re-verification of existing files.

## 6. Security

- Named pipe: DACL `SY + user SID`; no TCP/LAN; JSON-lines payload, 1 MiB/line cap.
- Session/creds: only in `%APPDATA%\tgdlrs`; the GUI saves `config.json` with a
  timestamped backup; nothing secret is logged; MCP output carries no secrets.
- No arbitrary shell execution; only spawns `tgdlrs-worker.exe` by absolute path.

## 7. UI surfaces

- `tgdlrs-login.ps1` (WinForms, bilingual EN/VI): appears when logged out; saves
  `api_id`/`api_hash`/phone; OTP/2FA stay typed in the terminal (hidden); detects
  successful login by the console exit code + session file.
- `Desktop\Telegram Downloader - Login.lnk`.
- CLI `tgdlrs <cmd> [--json]`.

## 8. Roadmap & current status

| Phase | Content | Status |
|---|---|---|
| P0 | Environment audit, toolchain, winsqlite3 | ✅ (GNU toolchain; SQLite 3.53.4; 123 GiB free) |
| P1 | Core: sqlite FFI, models, store, migrations | ✅ + tests |
| P2 | Resume engine (plan/loop/retry/control) | ✅ + tests |
| P3 | Telegram connect/login/resolve/scan/search | ✅ compiled; ✅ verified live (account 玉玲珑) |
| P4 | Worker + named pipe + recovery + autostart | ✅ smoke-tested; ✅ autostart registered |
| P5 | CLI (12 commands) + `--json` | ✅ |
| P6 | MCP 15 tools + structured outputs | ✅ live in OpenCode |
| P7 | GUI login + shortcut + `--force` account switch | ✅ tested (EN/VI, failure path verified) |
| P8 | **Download CHINT.7z.021/.022/.024/.025/.026 + verify** | 🟡 **in progress** (5 jobs enqueued; .021 downloading, ~95 MB at first check; see log) |
| P9 | `7z t` once all 26 parts exist | ⏳ blocked by P8 |
| P10 | Public GitHub repo, EN+VI docs/GUI | 🟡 in progress (docs/GUI bilingual; push happens **after** P8/P9 per decision) |

Notes from live operation (2026-11-14):

- `omegaxz` (first account) cannot resolve the private channel → `CHANNEL_INVALID`.
  Switching to the member account `玉玲珑` fixed it. The GUI gained a
  "log in again from scratch (--force)" switch for exactly this.
- Five placeholder jobs created via the MCP URL-only path would have shared
  `download.bin`; they were canceled and re-enqueued with explicit per-part
  names. A `placeholder_file_name` fix landed in the MCP server.
- One stale 27.5 MB partial `CHINT.7z.021.part` (owner: canceled placeholder job
  `1340664d`) is intentionally kept on disk.

## 9. Testing

- `cargo test --workspace` — **43 tests green** (36 core + 7 MCP), including:
  unaligned-tail truncation, pause/resume/retry via the DB, a `transition()`
  placeholder-binding regression (found by the lifecycle test), verify jobs,
  missing-parts detection, IPC round-trips, session snapshot round-trips/corruption.
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- `cargo fmt --all -- --check` — clean.
- `cargo build --release --workspace` — OK; binaries only depend on system DLLs.
- Live resume across a `daemon stop`/`start` cycle: verified (`.part` resumed at
  the aligned offset, no re-downloaded bytes).
- Not covered by automation (and therefore only claimed after live runs):
  full live download of the 5 parts, CDN redirect behavior (see §5).

## 10. CHINT execution plan

1. `tgdlrs login` as the channel member (`玉玲珑`) — **done**.
2. `telegram_search_files(chat=-1001188194766, query="CHINT")` → found
   `CHINT.7z.001…026` at message ids 1914…1939; parts are 4,175,429,632 bytes,
   `.026` is 1,415,085,203 bytes.
3. `telegram_find_missing_parts(1..26)` → missing exactly
   `.021 (msg 1934), .022 (1935), .024 (1937), .025 (1938), .026 (1939)`.
4. Enqueue exactly those five URLs with explicit names (see §8).
5. `telegram_list_downloads` to watch until all reach `VERIFIED`.
6. Final `telegram_verify_file` pass; then `7z t CHINT.7z.001` (test only).
7. Push the finished repo to GitHub (public, EN+VI) after P8/P9.
