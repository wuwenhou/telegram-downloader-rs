# Telegram Downloader RS â€” Káº¿ hoáº¡ch chi tiáº¿t (tiáº¿ng Viá»‡t)

TĂ i liá»‡u nĂ y mĂ´ táº£ **káº¿ hoáº¡ch thiáº¿t káº¿, phĂ¢n xá»­ lĂ½ vĂ  tráº¡ng thĂ¡i** cá»§a dá»± Ă¡n,
phá»¥c vá»¥ báº£o trĂ¬ vĂ  Ä‘Ă¡nh giĂ¡. Tráº¡ng thĂ¡i ghi nháº­n táº¡i ngĂ y **2026-11-14**.

---

## 1. Má»¥c tiĂªu

1. Táº£i cĂ¡c file Telegram ráº¥t lá»›n (bá»™ `CHINT.7z.001â€¦026` ~4,17 GB má»—i pháº§n,
   tá»•ng ~108,6 GB) vĂ o `D:\Downloads\Brave`, **khĂ´ng táº£i láº¡i** cĂ¡c pháº§n Ä‘Ă£ Ä‘á»§
   (`.001`â€“`.020` vĂ  `.023`, má»—i pháº§n Ä‘Ăºng 4.175.429.632 bytes).
2. Resume **tháº­t** á»Ÿ má»©c byte sau: rá»›t máº¡ng, kill tiáº¿n trĂ¬nh, Windows restart.
3. Enqueue/Ä‘iá»u khiá»ƒn tá»« CLI vĂ  tá»« OpenCode (MCP), khĂ´ng khĂ³a phiĂªn chat.
4. Worker cháº¡y ná»n Ä‘á»™c láº­p OpenCode, tá»± khĂ´i phá»¥c job dá»Ÿ dang, autostart
   lĂºc Ä‘Äƒng nháº­p Windows, khĂ´ng cáº§n Administrator.
5. SHA-256 xĂ¡c thá»±c tá»«ng pháº§n; file cuá»‘i chá»‰ Ä‘á»•i tĂªn sau khi khá»›p kĂ­ch thÆ°á»›c.
6. KhĂ´ng bao giá»: ghi Ä‘Ă¨ file tá»“n táº¡i khi chÆ°a khá»›p identity, ná»‘i byte tá»«
   nguá»“n khĂ¡c, xĂ³a `.part`.

## 2. YĂªu cáº§u & rĂ ng buá»™c

| # | YĂªu cáº§u | Quyáº¿t Ä‘á»‹nh |
|---|---|---|
| R1 | Core engine báº±ng Rust, khĂ´ng C toolchain | `winsqlite3.dll` FFI qua `libloading`; Grammers (pure Rust) |
| R2 | Windows x64 | `rust-toolchain.toml` â†’ `stable-x86_64-pc-windows-gnu` |
| R3 | Resume byte-level cho Telegram | `upload.getFile`, chunk 512 KiB, `skip_chunks`; tail khĂ´ng cháºµn chunk bá»‹ cáº¯t lĂ¹i |
| R4 | Session bá»n qua restart | `grammers-session` + `FileSession` JSON tá»± persist (atomic rename) |
| R5 | Queue + pause/resume/retry/cancel + FloodWait | Worker poll DB má»—i ~2 s; `DownloadControl` Ä‘áº·t tá»« tráº¡ng thĂ¡i DB |
| R6 | KhĂ´ng má»Ÿ port TCP | Named pipe `\\.\pipe\tgdlrs-<user>` + DACL user+SYSTEM; JSON-lines |
| R7 | CLI 12 lá»‡nh + `--json` | login/download/scan/status/list/pause/resume/cancel/retry/verify/doctor/daemon/mcp |
| R8 | MCP 15 tools, khĂ´ng block | `rmcp` tool_router; chá»‰ enqueue/controls |
| R9 | KhĂ´ng secrets trong chat/log/git | Session + creds chá»‰ á»Ÿ `%APPDATA%\tgdlrs`; `.gitignore` |

RĂ ng buá»™c mĂ´i trÆ°á»ng: khĂ´ng cĂ³ MSVC linker / Windows SDK / CMake / libsqlite3 C
â†’ dĂ¹ng GNU toolchain + MinGW-w64 local (`as`/`dlltool`); `cargo test` cáº§n
`TMP`/`TEMP` trá» tá»›i thÆ° má»¥c ghi Ä‘Æ°á»£c (`target\tmp`).

## 3. Kiáº¿n trĂºc tá»•ng thá»ƒ

```
â”Œâ”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”   DB writes (pause/resume/â€¦)   â”Œâ”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”
â”‚ tgdlrs CLI   â”‚â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â–¶â”‚                â”‚
â””â”€â”€â”€â”€â”€â”€â”¬â”€â”€â”€â”€â”€â”€â”€â”˜                                 â”‚  state.db      â”‚
       â”‚ IPC (named pipe, JSON-lines)              â”‚  (winsqlite3,  â”‚
       â–¼                                         â”‚   WAL)         â”‚
â”Œâ”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”   IPC (scan/resolve/search)    â”‚                â”‚
â”‚ tgdlrs-mcp   â”‚â—€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”‚                â”‚
â”‚ (stdio)      â”‚                                â””â”€â”€â”€â”€â”€â”€â”¬â”€â”€â”€â”€â”€â”€â”€â”€â”€â”˜
â””â”€â”€â”€â”€â”€â”€â”¬â”€â”€â”€â”€â”€â”€â”€â”˜                                       â”‚ poll ~2s
       â”‚ enqueue (SQLite)                              â–¼
â”Œâ”€â”€â”€â”€â”€â”€â”´â”€â”€â”€â”€â”€â”€â”€â”                             â”Œâ”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”
â”‚ tgdlrs-workerâ”‚â—€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”‚   (queue/control) â”‚
â”‚  (detached)  â”‚   Telegram MTProto          â””â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”˜
â””â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”˜
```

- **Worker** lĂ  tiáº¿n trĂ¬nh duy nháº¥t giá»¯ session Telegram; CLI/MCP ghi Ă½ Ä‘á»‹nh
  vĂ o SQLite; worker tá»± tháº¥y trong ~2 giĂ¢y.
- **MCP** gá»i Telegram chá»‰ qua worker (pipe); scan/search cĂ³ timeout phĂ­a client.
- CLI `login` táº¡m dá»«ng worker, cháº¡y client táº¡m, lÆ°u session rá»“i má»Ÿ láº¡i worker.

## 4. MĂ´ hĂ¬nh dá»¯ liá»‡u & tráº¡ng thĂ¡i

`%APPDATA%\tgdlrs\state.db` â€” cĂ¡c báº£ng: `downloads`, `download_parts`,
`download_events`, `telegram_sources`, `verification_results`, `settings`,
`schema_migrations`.

VĂ²ng Ä‘á»i: `QUEUED â†’ RESOLVING â†’ DOWNLOADING â†’ VERIFYING â†’ COMPLETED â†’ VERIFIED`
(nhĂ¡nh lá»—i: `RETRY_WAIT â†’ QUEUED` hoáº·c `FAILED`; `PAUSED` tá»« háº§u háº¿t tráº¡ng thĂ¡i;
`CANCELED`; verify láº¡i cĂ³ thá»ƒ tráº£ `FAILED`).

## 5. Thiáº¿t káº¿ resume (Ä‘iá»ƒm then chá»‘t)

1. Má»—i job dĂ¹ng `.part` = `<tĂªn>.part` (hoáº·c `<tĂªn>.<id8>.part` khi trĂ¹ng).
2. `plan_resume(part_len, expected_size)`:
   - rá»—ng â†’ tá»« 0;
   - cháºµn 512 KiB â†’ tiáº¿p tá»¥c táº¡i offset;
   - láº» â†’ cáº¯t `.part` lĂ¹i vá» chunk cháºµn gáº§n nháº¥t, tÄƒng `redownloaded_bytes`;
   - `.part` lá»›n hÆ¡n expected â†’ bá», ghi nháº­n.
3. Nguá»“n: `upload.getFile` offset cháºµn 4 KiB; Grammers
   `iter_download().chunk_size(512 KiB).skip_chunks(offset / 512 KiB)`;
   offset láº» qua `pending_skip` (táº£i chunk chá»©a offset rá»“i bá» pháº§n Ä‘áº§u).
4. Sau lá»—i máº¡ng: táº¡o láº¡i `DownloadIter` táº¡i offset hiá»‡n táº¡i (iterator Grammers
   cháº¿t sau lá»—i); backoff lÅ©y thá»«a + jitter, tráº§n 60 s; FloodWait tĂ´n trá»ng
   Ä‘Ăºng thá»i gian server yĂªu cáº§u.
5. HoĂ n thĂ nh: kiá»ƒm tra `final_path` tá»“n táº¡i â†’ verify; khá»›p â†’ rename no-overwrite;
   lá»‡ch â†’ job FAILED, file tá»“n táº¡i khĂ´ng Ä‘á»¥ng; `.part` giá»¯ hoáº·c quarantine
   `<part>.bad-<ts>` khi verify sau hash tháº¥t báº¡i.

### Giá»›i háº¡n Ä‘Ă£ biáº¿t (trung thá»±c)

- Náº¿u Telegram tráº£ **CDN redirect**, Grammers (build `cdn_supported=false`) panic;
  chĂºng ta báº¯t panic vĂ  tráº£ lá»—i **khĂ´ng retryable** â€” khĂ´ng há»— trá»£ CDN.
- FloodWait hĂ ng giá» Ä‘Æ°á»£c tĂ´n trá»ng (Æ°u tiĂªn an toĂ n tĂ i khoáº£n).
- `skip_verified` bá» táº£i láº¡i, khĂ´ng bá» verify-láº¡i file Ä‘Ă£ tá»“n táº¡i.

## 6. Báº£o máº­t

- Named pipe: DACL `SY + user SID`; khĂ´ng TCP/LAN; payload JSON-lines, tá»‘i Ä‘a 1 MiB.
- Session/creds chá»‰ á»Ÿ `%APPDATA%\tgdlrs`; GUI ghi `config.json` cĂ³ backup;
  khĂ´ng log bĂ­ máº­t; MCP khĂ´ng tráº£ secrets.
- KhĂ´ng shell ngoĂ i; chá»‰ spawn `tgdlrs-worker.exe` tuyá»‡t Ä‘á»‘i Ä‘Æ°á»ng dáº«n.

## 7. Giao diá»‡n ngÆ°á»i dĂ¹ng

- `tgdlrs-login.ps1` (WinForms, song ngá»¯ EN/VI): hiá»‡n khi chÆ°a Ä‘Äƒng nháº­p;
  lÆ°u `api_id/api_hash/phone`; OTP/2FA váº«n gĂµ áº©n trong cá»­a sá»• Ä‘en; tá»± phĂ¡t hiá»‡n
  Ä‘Äƒng nháº­p thĂ nh cĂ´ng qua exit code + session file; cĂ³ nĂºt **Ä‘Äƒng nháº­p láº¡i tá»« Ä‘áº§u
  (--force)** Ä‘á»ƒ Ä‘á»•i tĂ i khoáº£n; nhá»› ngĂ´n ngá»¯ trong `config.json` (`gui_lang`).
- Shortcut `Desktop\Telegram Downloader - Login.lnk`.
- CLI `tgdlrs <cmd> [--json]`.

## 8. Lá»™ trĂ¬nh & tráº¡ng thĂ¡i

| Pha | Ná»™i dung | Tráº¡ng thĂ¡i |
|---|---|---|
| P0 | Audit mĂ´i trÆ°á»ng, toolchain, winsqlite3 | âœ… (GNU toolchain; SQLite 3.53.4; 123 GiB trá»‘ng) |
| P1 | Core: sqlite FFI, models, store, migrations | âœ… + tests |
| P2 | Resume engine (plan/loop/retry/control) | âœ… + tests |
| P3 | Telegram connect/login/resolve/scan/search | âœ… compile; âœ… cháº¡y live (account ç‰ç²ç‘) |
| P4 | Worker + named pipe + recovery + autostart | âœ… smoke-tested; âœ… autostart Ä‘Äƒng kĂ½ |
| P5 | CLI 12 lá»‡nh + `--json` | âœ… |
| P6 | MCP 15 tools + structured output | âœ… live trong OpenCode |
| P7 | GUI login + shortcut + Ä‘á»•i tĂ i khoáº£n --force | âœ… test (EN/VI, failure path) |
| P8 | **Táº£i CHINT.7z.021/022/024/025/026 + verify** | đŸŸ¡ **Ä‘ang cháº¡y** (5 job enqueued; .021 Ä‘ang táº£i, ~95 MB táº¡i láº§n kiá»ƒm Ä‘áº§u) |
| P9 | `7z t` khi Ä‘á»§ 26 pháº§n | â³ phá»¥ thuá»™c P8 |
| P10 | GitHub repo cĂ´ng khai, song ngá»¯ EN/VI | đŸŸ¡ Ä‘ang lĂ m (docs/GUI song ngá»¯; push **sau** khi P8/P9 xong) |

Ghi chĂº váº­n hĂ nh thá»±c táº¿ (2026-11-14):

- `omegaxz` (account Ä‘áº§u) **khĂ´ng** resolve Ä‘Æ°á»£c private channel â†’ `CHANNEL_INVALID`.
  Chuyá»ƒn sang account thĂ nh viĂªn `ç‰ç²ç‘` lĂ  háº¿t lá»—i. GUI cĂ³ thĂªm nĂºt
  "Ä‘Äƒng nháº­p láº¡i tá»« Ä‘áº§u (--force)" Ä‘Ăºng cho tĂ¬nh huá»‘ng nĂ y.
- 5 job placeholder táº¡o qua MCP URL-only trĂ¹ng tĂªn `download.bin`; Ä‘Ă£ cancel vĂ 
  re-enqueue vá»›i tĂªn riĂªng tá»«ng pháº§n; MCP Ä‘Ă£ Ä‘Æ°á»£c sá»­a: `placeholder_file_name`.
- Má»™t `.part` 27,5 MB cĂ²n sĂ³t láº¡i (`CHINT.7z.021.part`, chá»§ sá»Ÿ há»¯u lĂ  job
  placeholder Ä‘Ă£ cancel `1340664d`) Ä‘Æ°á»£c **giá»¯ nguyĂªn** theo Ä‘Ăºng nguyĂªn táº¯c.

## 9. Kiá»ƒm thá»­

- `cargo test --workspace` â€” **43 tests xanh** (36 core + 7 MCP).
- `cargo clippy --workspace --all-targets -- -D warnings` â€” sáº¡ch.
- `cargo fmt --all -- --check` â€” sáº¡ch.
- `cargo build --release --workspace` â€” OK; binary chá»‰ phá»¥ thuá»™c DLL há»‡ thá»‘ng.
- Resume thá»±c táº¿ qua chu ká»³ `daemon stop` â†’ `start`: âœ… (resume tá»« `.part`,
  khĂ´ng táº£i láº¡i byte nĂ o).
- ChÆ°a automation: táº£i Ä‘áº§y Ä‘á»§ 5 pháº§n live vĂ  hĂ nh vi CDN redirect (má»¥c 5) â€”
  sáº½ xĂ¡c nháº­n sau P8.

## 10. Káº¿ hoáº¡ch thá»±c thi CHINT

1. `tgdlrs login` vá»›i account thĂ nh viĂªn (`ç‰ç²ç‘`) â€” **xong**.
2. `telegram_search_files(chat=-1001188194766, query="CHINT")` â†’ tĂ¬m tháº¥y
   `CHINT.7z.001â€¦026` táº¡i message id 1914â€¦1939; cĂ¡c pháº§n 4.175.429.632 bytes,
   `.026` = 1.415.085.203 bytes.
3. `telegram_find_missing_parts(1..26)` â†’ thiáº¿u Ä‘Ăºng
   `.021 (msg 1934), .022 (1935), .024 (1937), .025 (1938), .026 (1939)`.
4. Enqueue Ä‘Ăºng 5 link vá»›i tĂªn rĂµ rĂ ng.
5. `telegram_list_downloads` theo dĂµi tá»›i `VERIFIED`.
6. `telegram_verify_file` láº§n cuá»‘i; rá»“i `7z t CHINT.7z.001` (chá»‰ test).
7. Push repo cĂ´ng khai lĂªn GitHub sau P8/P9.
