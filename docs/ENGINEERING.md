# Engineering Overview — Telegram Downloader RS

Technical reference for engineers and AI agents working on this codebase.
For usage, see `USER-GUIDE.en.md`; for project history, see `PROJECT-PLAN.en.md`.

**Platform:** Windows x64 · **Language:** Rust 2021 · **License:** MIT ·
**Release:** [`v0.1.0`](https://github.com/wuwenhou/telegram-downloader-rs/releases/tag/v0.1.0)

---

## 1. Product summary

Telegram Downloader RS is a resumable Telegram MTProto download manager:
Rust core engine + persistent background worker + SQLite state + named-pipe
IPC + CLI + OpenCode MCP server + bilingual login GUI.

No Python, no Node, no C toolchain — the only native dependency is SQLite,
bound through a custom minimal FFI layer over `winsqlite3.dll`.

## 2. Workspace layout

```
crates/
  tgdlrs-core    — engine: download, parallelism, resume, verification,
                   Telegram integration, SQLite, IPC,
                   archive streaming + upload + caption repair
  tgdlrs-worker  — detached daemon: queue dispatch, verification, autostart
  tgdlrs-cli     — tgdlrs.exe: subcommands
                   (login/download/scan/status/list/pause/resume/retry/verify/
                    doctor/daemon/mcp/upload/fix-captions)
  tgdlrs-mcp     — MCP server implementation (stdio transport)
```

Release binaries ship via GitHub releases: `tgdlrs.exe` (CLI + `tgdlrs.exe mcp`)
and `tgdlrs-worker.exe`. The binaries also host the login GUI helpers in
`extras/` (`tgdlrs-login.ps1`, `tgdlrs-notify.ps1`).

## 3. Telegram layer (Grammers MTProto)

`grammers-client` over `grammers-mtsender`, session persisted at
`%APPDATA%\tgdlrs\telegram.session.json` (atomic temp-file + rename writes).

- **Peer resolution** (`tgdlrs-core/src/telegram/mod.rs`):
  - Private channel links: `t.me/c/<internal_id>` (Bot API dialog ↔ channel id mapping via `PeerId::channel` / `PeerId::from_bot_api_dialog_id`).
  - Public usernames / `t.me/<name>` via `resolve_username`.
  - Invite links `t.me/+<hash>` / `t.me/joinchat/<hash>`:
    `messages.importChatInvite` (`accept_invite_link`); on failure (e.g.
    `INVITE_HASH_EXPIRED` for an already-member account) falls back to
    `messages.checkChatInvite` and reads the `ChatInviteAlready` arm to
    obtain the channel id + access hash directly.
  - Last-resort heuristic: iterate dialogs and pick a channel whose most
    recent posts are `.edz` documents (only if unambiguous).

- **Downloads**: `iter_download()` with fixed 512 KiB chunk sources,
  parallel range streams (`parallel.rs`).

- **Uploads**: `client.upload_stream()` (big files upload in parallel
  chunk workers) + `InputMessage::document()` / `.file()` with
  `text(caption)`, `DocumentAttributeFilename` = exact file name.

- **FloodWait handling**: `InvocationError::Rpc` with code 420
  (`FLOOD_WAIT_N`) maps to a typed `Error::FloodWait(secs)` (also recovered
  from free-text rpc errors, including mid-upload `SaveFilePart` failures).
  Flood waits are always honored exactly (sleep), never consumed from the
  retry budget; a separate cap of 8 flood-waits per file/operation applies.
  Other transient errors use exponential backoff with jitter.

## 4. Download engine & byte-level resume

### 4.1 Fixed-size chunks and sidecar tracking

- Chunk size: **512 KiB** for both serial and parallel paths.
- Per file, the worker/engine persists progress to two artifacts:
  - `<file>.part` — the partial bytes; byte-exact for the verified ranges.
  - `<file>.part.ranges.json` — explicit contiguous ranges written
    (e.g. `{"ranges":[start,end,…]}`).

### 4.2 Resume correctness (read the sidecar, not the file length)

The engine never trusts `.part` file length. Resume decisions are based on
the sidecar's recorded ranges:

- Ranges that extend the file must be contiguous and aligned.
- On sidecar/`.part` mismatch (e.g. partial written but sidecar lost,
  concurrent writers, or a sparse file from a killed parallel run), the
  ranges for that file are reset to zero — the file downloads fresh.
- Unaligned tails: the unaligned tail chunk re-downloads; bytes beyond the
  aligned prefix are never trusted.
- End-to-end consequence: final SHA-256 of every file is deterministic and
  matches the source digest exactly.

### 4.3 Verification and quarantine

- Streaming SHA-256 (64 hex) verified before the file is renamed into
  place; `verify` column on the job gates this.
- On failure: the `.part` is renamed to `.part.bad-<timestamp>` (kept,
  never deleted) and the job is re-queued with a fresh sidecar.

### 4.4 Parallel range engine

- Up to `max_parallel_streams` streams per file (default 4).
- Each stream owns a contiguous, chunk-aligned range; per-range progress
  persisted in the sidecar; per-range resume markers kept in memory.
- Cross-run safety: `.ranges.json` mismatch triggers a full range reset.

### 4.5 Concurrency model (worker)

- `max_concurrent_files` slots (default 1); jobs claimed by priority,
  `next_retry_at` backoff respected.
- Disk-reserve check before starting (blocks jobs when free space is below
  the configured reserve).

## 5. Worker & control plane

### 5.1 Persistent background worker

- Autostart via `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\tgdlrs-worker`.
- Launched detached (no console, stdio discarded) via `spawn_worker_detached`
  (`CREATE_NO_WINDOW`); the launcher in `extras/tgdlrs-launch.vbs` uses
  Explorer → wscript for full detachment from hostile shells.
- Never holds a TCP port open; it can die and be restarted at any time
  without losing partials (`.part` + sidecar).

### 5.2 Named-pipe IPC

- Named pipe `\\.\pipe\tgdlrs-<user>` with interactive-user DACL.
- Length-prefixed JSON `Request`/`Response` (`tgdlrs-core/src/ipc.rs`).
- `Ping`, `Status`, `Download`, `Pause`, `Resume`, `Retry`, `Cancel`,
  `Verify`, `ResolveLink`, `ScanChat`, `SearchFiles`, `Stop`, `Checksum`.
- MCP and CLI reach the worker through this pipe; SQLite is used directly
  for queue state queries.

### 5.3 SQLite schema (state.db)

| Table | Purpose |
|---|---|
| `downloads` | One row per download job (state, attempt, bytes, paths, SHA) |
| `download_parts` | Per-range progress for parallel chunks |
| `download_events` | Event journal (evicted to ~50/download) |
| `verification_results` | Every verify attempt, expected vs actual |
| `telegram_sources` | Resolved sources (cache of scanned links) |
| `settings` | Generic key/value (watermarks, counters) |
| `upload_jobs` | One row per `.edz` posted to Telegram (state, message id, attempts) |
| `dest_inventory` | Every `.edz` document known to exist in the destination channel (name, size, sha256, message id) |
| `schema_migrations` | Schema version bookkeeping (currently v4) |

`dest_inventory` enables the destination pre-scan (§7.2) and is updated by
both the upload pipeline and the caption repair tool.

## 6. Archive-wide streaming upload (`tgdlrs upload`)

The archive `CHINT.7z.001 … .026` is a single LZMA2 solid stream; per-file
extraction would re-decode from the block start (O(n²)). Instead, the tool
decodes the archive **once** and streams the bytes straight into Telegram.

### 6.1 Manifest

`7z l -slt <first_volume>` (via `archive::list_entries`) produces the
ordered entry list — `.edz` files only, in archive order — with each size
and CRC-32. This list is the de-multiplex stride table.

### 6.2 De-multiplex stream

`7z x -so -y` writes every archive member's bytes to stdout in listing
order. `StreamingArchive` (`archive.rs`) spawns it, pipes stdout/stderr and
drains stderr into a capped buffer for diagnostics. The Rust side splits
the stream into per-file `Vec<u8>` buffers using the known sizes — zero
temp files, O(total bytes).

### 6.3 Integrity guard (before anything leaves the machine)

Each de-multiplexed file is **CRC-32 (IEEE)** checked against the listing.
Any misalignment → the run aborts before a single byte reaches Telegram
("refuse to publish garbage"). SHA-256 is computed during the read; CRC
protects the demux, SHA-256 identifies the artifact.

### 6.4 Caption (OUMIJIA EDZ template replica)

```
EPLAN EDZ | CHINT
Part: CHT.101041
File: CHT.101041.edz
SHA-256: <64 lowercase hex>
#CHINT #EPLAN #EDZ #EDZID_<sha256[:16]>
```

`Part:` = file name minus the `.edz` suffix. `EDZID_` = first 16 hex of
SHA-256 (case-insensitive compare with the bot's `a['sha256'][:16]`).
Caption ≤ 1024 chars, no parse mode (plain text),
`force_document` equivalent (`InputMessage::file()`), exact file name
attribute.

### 6.5 Posting loop

- Per file: skip if already known posted; else CRC-check → SHA-256 →
  caption → `upload_stream` → `send_message` → record in `upload_jobs` →
  sleep 250 ms (`--delay-ms`).
- `--parallel 2` workers share a bounded MPSC channel (3 jobs in flight);
  decode stays sequential (solid stream), upload goes through two workers.
- Retry per file: FloodWait → exact sleep (separate budget of 8); other
  transient → exponential backoff, cap of 5 attempts (`--max-attempts`).
- Permanent failure → dump to `%TEMP%\tgdlrs-upload-failed\` + DB row,
  continue with the next file. Fatal stream-side failure → abort the run.
- Every successful post writes `upload_jobs(state='posted')` with message id
  before the next file is touched ⇒ crash/resume safe.

### 6.6 Resume semantics

Interrupt the process at any point; on the next run the destination
pre-scan (§7.2) + `upload_jobs` identify every already-posted `.edz` by
name (and SHA where available), skip them instantly, and continue from the
first unposted file. Nothing is ever re-posted.

## 7. Destination dedup & caption repair

### 7.1 `dest_inventory` (name+size, caption-derived)

- Pre-scan uses a server-side `InputMessagesFilterDocument` search instead
  of walking the whole history. Backwards-descending with `offset_id`.
- Progress is **checkpointed**: `dest_scan_resume` (oldest reached) and
  `dest_scan_watermark` (highest seen once complete) live in `settings`.
  A full scan is resumable across crashes; partial progress is retained.
- On later runs the inventory is refreshed incrementally from the stored
  watermark. `dest_inventory` keeps the union of everything known to exist.

### 7.2 `tgdlrs fix-captions`

1. **Audit** (`collect_caption_audit`): scan every `.edz` document, classify
   captions (valid `SHA-256: <64hex>` line + `#EDZID_…` present → OK; else
   flagged), split into `flagged_chint` (CHINT archive member, by name+size)
   and `flagged_other` (left untouched).
2. **Backup**: every flagged CHINT caption written to
   `caption-backup-<timestamp>.json` *before* the first edit.
3. **Repair** (`repair_caption`): keep the leading `EPLAN EDZ | …`, `Part:`,
   `File:` lines byte-for-byte; replace/insert the `SHA-256:` line (placed
   right after `File:`) and the `#… #EPLAN #EDZ #EDZID_<16hex>` hashtag line;
   caption capped at 1024 chars like the reference template.
4. **Edit**: `messages.EditMessage` as the post author (needs the posting
   account — 玉玲珑 / omegaxz). FloodWait honored; per-message retries;
   permanent failures reported separately.
5. **Report**: `caption-fixed-<timestamp>.json` with per-message results.

## 8. MCP + OpenCode

`opencode.json` registers `mcp.servers["telegram-downloader"]` →
`tgdlrs.exe mcp` (stdio). Tools cover queue enqueue/control/verify,
chat scanning/search, diagnostics, and are used by OpenCode agents for
Telegram operations. The MCP server never downloads/uploads by itself;
it talks to the worker over the named pipe.

## 9. Design principles

1. Deterministic byte-level resume; never trust lengths.
2. Dedup by exact bytes/hash (SHA-256), never by hope.
3. Processes die — state never does: SQLite + per-file `.ranges.json` sidecars
   + checkpoints at every stage.
4. Verify before publishing; refuse to post unverified bytes.
5. Never delete partials; rename into place only after verification.
6. FloodWait is a signal, not an error.
7. Append-mostly channel model: removed files are not re-posted or re-deleted;
   dedup keys are name+size/SHA, not absence-of-evidence.
8. Watchdogs, not daemons-that-never-die: scheduled tasks restart crashed
   helpers; the engine itself is stateless across restarts.

## 10. Troubleshooting quick reference

| Symptom | Likely cause | Fix |
|---|---|---|
| `FloodWait` | account rate limit | wait the exact seconds; reduce `--parallel`/`--delay-ms` |
| `INVITE_HASH_EXPIRED` | invite link expired | account must be a channel member; use dialog auto-detection or `--dest-chat-id` |
| `CHANNEL_INVALID` (omegaxz) | account not a member | join the channel first |
| `verification failed` on resume | corrupted sparse `.part` | re-queue; engine resets ranges; never resumes on bad prefix |
| `partial file … bytes mismatch` | interrupted write / foreign file | move aside and retry; `.part.bad-*` kept for forensics |
| `destination scan: hit the … document cap` | channel larger than scan cap | a full first scan may need repeated runs; it resumes from `dest_scan_resume` until complete |
| Captions missing `#EDZID_` | file posted manually | `tgdlrs fix-captions --yes` |
| Worker dead after session teardown | shell killed it | `tgdlrs daemon start`; autostart re-launches at login |

## 11. Performance notes

- Full decode of the split archive: ~4–5 minutes for 247 GB uncompressed
  (LZMA2, multi-threaded) — CRC-verified per entry.
- Upload throughput is network-bound (~1.7–2 MB/s typical uplink):
  ≈ 9–17 files/min for ~10 MB files. `--parallel 2` helps hide Telegram
  ACK latency; going beyond 2 risks FloodWait with diminishing returns.
- `.edz` files average ~10 MB; 22,665 files ≈ 230 GB total.

---

*See also:* `README.md` (features), `USER-GUIDE.en.md` (usage),
`PROJECT-PLAN.en.md` (history & roadmap).
