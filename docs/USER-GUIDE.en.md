# Telegram Downloader RS — User guide (English)

Download very large files (>4 GB) from Telegram with **real byte-level resume**:
a persistent background worker, controllable from the CLI or through **MCP** in OpenCode.

## 0. Requirements

- Windows 10/11 x64.
- `api_id` / `api_hash` from <https://my.telegram.org> (*API development tools*).
- Enough free disk space (the CHINT.7z set needs ~109 GB).
- No Administrator rights are required.

## 1. Install & diagnose

```bat
C:\Users\OWG\AppData\Local\tgdlrs\bin\tgdlrs.exe doctor
```

`doctor` reports: config, download folder `D:\Downloads\Brave`, free space,
writability, SQLite version, worker, session and autostart status.

## 2. Log in once (required before any download)

Fastest: double-click **Telegram Downloader - Login** on your Desktop
(`%LOCALAPPDATA%\tgdlrs\tgdlrs-login.cmd`). A small bilingual window appears:

1. Fill in **API ID** (number), **API Hash** (32 hex, hidden), **phone** (`+84…`).
2. Click **① Save and send OTP** — the CLI window `tgdlrs login` opens.
3. In the black window type the OTP Telegram sends to the app → Enter →
   your 2FA password if you have one → Enter. Nothing is echoed.
4. The window turns green: **LOGIN SUCCESSFUL**.

Terminal way:

```bat
C:\Users\OWG\AppData\Local\tgdlrs\bin\tgdlrs.exe login
```

> To switch accounts, use **Log in again from scratch (--force)** in the GUI
> (or `tgdlrs login --force` in a terminal). The old session is backed up
> automatically to `telegram.session.json.bak-<timestamp>`.

> The session is stored at `%APPDATA%\tgdlrs\telegram.session.json`.
> Never share it. Never paste `api_id`/`api_hash`/OTP/2FA into chat.

## 3. Download files / the CHINT.7z set

Enqueue one link:

```bat
tgdlrs download "https://t.me/c/1188194766/1914"
```

The background worker (autostart with Windows) picks the job up, downloads
sequentially (`max_concurrent_files = 1`), resumes `.part` files, retries with
backoff + FloodWait, then SHA-256-verifies and only then renames `.part` to the
final name.

Inside OpenCode (MCP `telegram-downloader`) just say, for example:

- «Scan chat `-1001188194766` and show which CHINT parts exist»
- «Enqueue the missing CHINT parts (do not re-download .001–.023)»
- «Show me the progress of the downloads»
- «Verify the downloaded CHINT files»

The agent will call `telegram_scan_chat` / `telegram_search_files` /
`telegram_batch_download` / `telegram_list_downloads` / `telegram_verify_file`.

## 4. Watch & control

```bat
tgdlrs status            rem worker + queue + total bytes
tgdlrs list              rem jobs (8-char id prefix, state, %)
tgdlrs list --state RETRY_WAIT
tgdlrs status --json     rem machine-readable
tgdlrs pause  <id>       rem worker notices within ~2 s; .part kept
tgdlrs resume <id>
tgdlrs retry  <id>       rem FAILED/CANCELED → QUEUED, resumes the .part
tgdlrs cancel <id>       rem keeps the .part, job becomes CANCELED
tgdlrs verify <id>       rem size + SHA-256, writes verification_results
tgdlrs verify "D:\Downloads\Brave\CHINT.7z.021" --path --expected-size 4175429632
```

- `<id>` = full id or any unique prefix.
- `.part` files are **never deleted and never concatenated**: an unaligned tail
  (bytes not ending on a 512 KiB chunk) is truncated back to the last chunk
  boundary and re-downloaded (`redownloaded_bytes` reports the cost).
- If the final file already exists, the rename only happens when size *and*
  SHA-256 match; otherwise the job fails and the file is **not overwritten**.

## 5. When all 26 parts exist

```bat
"7z.exe" t D:\Downloads\Brave\CHINT.7z.001
```

(test only — do **not** extract). 7-Zip can be downloaded separately or found
in the portable MinGW-w64 distribution (`E:\VScode_Opencode\Tools\mingw64\mingw64\bin\7z.exe`).

## 6. Autostart & the worker

- `tgdlrs daemon start` — run detached (no console), recovers interrupted jobs.
- `tgdlrs daemon stop` — graceful stop (a running download finishes the current chunk first; `.part` kept).
- `tgdlrs daemon status` — pid/version/authorized.
- `tgdlrs daemon install-autostart` — register `HKCU\…\Run\tgdlrs-worker` (per-user, no admin).
- `tgdlrs daemon remove-autostart`

## 7. MCP in OpenCode

MCP server **telegram-downloader** (`tgdlrs mcp`), 15 tools:
`telegram_login_status`, `telegram_resolve_link`, `telegram_scan_chat`,
`telegram_search_files`, `telegram_download`, `telegram_batch_download`,
`telegram_list_downloads`, `telegram_download_status`, `telegram_pause_download`,
`telegram_resume_download`, `telegram_cancel_download`, `telegram_retry_download`,
`telegram_verify_file`, `telegram_find_missing_parts`, `telegram_system_diagnostics`.

The tools only enqueue/control jobs; the worker does the downloading, so the
chat is never blocked. Errors are JSON `{"kind","message"}` (e.g. `flood_wait`).

## 8. Troubleshooting

| Symptom | Fix |
|---|---|
| `worker is not running` | `tgdlrs daemon start` |
| `telegram session is not authorized` | `tgdlrs login` |
| `api_id is missing` | put it in config.json or set User env vars `TGDLRS_API_ID` / `TGDLRS_API_HASH` |
| CHANNEL_INVALID | the logged-in account is **not a member** of the private channel — log in with the member account (GUI → Log in again from scratch) |
| FloodWait | wait — the worker sleeps exactly as long as Telegram demands, then continues |
| Verification failed | job FAILED, corrupt `.part` quarantined to `.part.bad-<ts>`; `tgdlrs retry <id>` |
| Disk below the 10 GiB reserve | worker stops accepting new jobs |
| `cargo test` can't create temp dirs | point `TMP`/`TEMP` at `target\tmp` |

## 9. Security

- Session/`api_hash`/OTP/2FA never leave your machine, never reach the logs or git.
- Named pipe `\\.\pipe\tgdlrs-<user>` is restricted to the current user + SYSTEM
  (explicit DACL). No TCP port is ever opened.
- `.gitignore` covers `telegram.session.json`, `config.json`, `*.part`, `target/`.

## 10. Uninstall

```bat
tgdlrs daemon stop
tgdlrs daemon remove-autostart
rmdir /s /q "%LOCALAPPDATA%\tgdlrs\bin"
rem session/config live in %APPDATA%\tgdlrs — delete manually for a full sign-out
```

Then remove `telegram-downloader` from `opencode.json` if you no longer use it.
