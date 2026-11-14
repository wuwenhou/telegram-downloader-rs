# Telegram Downloader RS — Kế hoạch chi tiết

Tài liệu này mô tả **kế hoạch thiết kế, phân xử lý và trạng thái** của dự án,
phục vụ bảo trì và đánh giá. Đối tượng: người bảo trì và agent (OpenCode).

Trạng thái hiện tại ghi nhận theo ngày **2026-11-14**.

---

## 1. Mục tiêu

1. Tải các file Telegram rất lớn (bộ `CHINT.7z.001…026` ~4,17 GB mỗi phần,
   tổng ~108,6 GB) vào `D:\Downloads\Brave`, **không tải lại** các phần đã đủ.
2. Resume **thật** ở mức byte sau: rớt mạng, kill tiến trình, Windows restart.
3. Enqueue/điều khiển từ CLI và từ OpenCode (MCP), không khóa phiên chat.
4. Worker chạy nền độc lập OpenCode, tự khôi phục job dở dang, autostart
   lúc đăng nhập Windows, không cần Administrator.
5. SHA-256 xác thực từng phần; file cuối chỉ đổi tên sau khi khớp kích thước.
6. Không bao giờ: ghi đè file tồn tại khi chưa khớp identity, nối byte từ
   nguồn khác, xóa `.part`.

## 2. Yêu cầu & ràng buộc

| # | Yêu cầu | Quyết định |
|---|---|---|
| R1 | Core engine bằng Rust, không C toolchain | `winsqlite3.dll` FFI qua `libloading`; Grammers (pure Rust) |
| R2 | Windows x64 | `rust-toolchain.toml` → `stable-x86_64-pc-windows-gnu` |
| R3 | Resume byte-level cho Telegram | `upload.getFile`, chunk 512 KiB, `skip_chunks`; tail không chẵn chunk bị cắt lùi |
| R4 | Session bền qua restart | `grammers-session` + `FileSession` JSON tự persist (atomic rename) |
| R5 | Queue + pause/resume/retry/cancel + FloodWait | Worker poll DB mỗi ~2 s; `DownloadControl` đặt từ trạng thái DB |
| R6 | Không mở port TCP | Named pipe `\\.\pipe\tgdlrs-<user>` + DACL user+SYSTEM; JSON-lines |
| R7 | CLI: login/download/scan/status/list/pause/resume/retry/verify/doctor/daemon/mcp | Có đủ, `--json` ở hầu hết lệnh |
| R8 | MCP 15 tools, không block, structured I/O | `rmcp` tool_router; chỉ enqueue/controls; `tools/list`-level latency |
| R9 | Không secrets trong chat/log/git | Session + creds chỉ ở `%APPDATA%\tgdlrs`; `.gitignore` |

Ràng buộc môi trường: MSVC linker / Windows SDK / CMake / libsqlite3 C đều
thiếu → chọn GNU toolchain + MinGW-w64 local (`as`/`dlltool`); `RUSTFLAGS`
không cần vì không biên dịch C. `cargo test` cần `TMP`/`TEMP` trỏ tới thư mục
ghi được (`target\tmp`).

## 3. Kiến trúc tổng thể

```
┌──────────────┐   DB writes (pause/resume/…)   ┌────────────────┐
│ tgdlrs CLI   │────────────────────────────────▶│                │
└──────┬───────┘                                 │  state.db      │
       │ IPC (named pipe, JSON-lines)            │  (winsqlite3,  │
       ▼                                        │   WAL)         │
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

- **Worker** là tiến trình duy nhất giữ session Telegram; CLI/MCP ghi ý định
  vào SQLite; worker tự thấy trong ~2 giây (pause/resume/cancel).
- **MCP** gọi Telegram chỉ qua worker (pipe); mọi thao tác dài (scan/search)
  có timeout phía client (120 s); MCP không bao giờ tải file.
- **CLI `login`** tạm dừng worker, chạy client tạm, lưu session rồi mở lại worker.

## 4. Mô hình dữ liệu & trạng thái

`%APPDATA%\tgdlrs\state.db` — các bảng: `downloads`, `download_parts`,
`download_events`, `telegram_sources`, `verification_results`, `settings`,
`schema_migrations`.

Vòng đời: `QUEUED → RESOLVING → DOWNLOADING → VERIFYING → COMPLETED → VERIFIED`
(nhánh lỗi: `RETRY_WAIT → QUEUED` hoặc `FAILED`; `PAUSED` từ hầu hết trạng thái;
`CANCELED` từ các trạng thái cho phép; verify lại sau có thể trả FAILED).

## 5. Thiết kế resume (điểm then chốt)

1. Mỗi file có `.part` = `<tên>.part` (hoặc `<tên>.<id8>.part` nếu trùng).
2. `plan_resume(part_len, expected_size)`:
   - rỗng → từ 0;
   - chẵn 512 KiB → tiếp tục tại offset;
   - lẻ → cắt `.part` lùi về chunk chẵn gần nhất, tăng `redownloaded_bytes`;
   - `.part` lớn hơn expected → bỏ (0 byte, ghi nhận).
3. Nguồn chunks: `upload.getFile` offset phải chẵn 4 KiB; Grammers
   `iter_download().chunk_size(512 KiB).skip_chunks(offset / 512 KiB)`;
   offset lẻ được xử lý `pending_skip` (tải chunk chứa offset rồi bỏ phần đầu).
4. Sau lỗi mạng: tạo lại `DownloadIter` tại offset hiện tại (iterator Grammers
   chết sau lỗi); backoff lũy thừa + jitter, trần 60 s; FloodWait tôn trọng
   đúng thời gian server yêu cầu.
5. Hoàn thành: kiểm tra `final_path` tồn tại → verify; khớp → rename no-overwrite;
   lệch → không đụng file, job FAILED; `.part` giữ nguyên hoặc quarantine
   `<part>.bad-<ts>` khi verify sau hash thất bại.

### Giới hạn đã biết (ghi trung thực)

- Nếu Telegram trả **CDN redirect** cho file, Grammers (build `cdn_supported=false`)
  panic; chúng ta bắt panic và trả lỗi **không retryable** rõ ràng — không hỗ trợ CDN.
- FloodWait kéo dài hàng giờ được tôn trọng (an toàn cho tài khoản).
- `skip_verified` chỉ bỏ tải lại, không bỏ verify-lại file đã tồn tại.

## 6. Bảo mật

- Named pipe: DACL `SY + user SID`; không `TCP`/`LAN`; payload JSON-lines, tối đa 1 MiB/line.
- Session/ creds: chỉ trong `%APPDATA%\tgdlrs`; `tgdlrs-login.ps1` (GUI) ghi
  `config.json` có backup; không log bí mật; MCP output không chứa secrets.
- Không shell ngoài; chỉ spawn `tgdlrs-worker.exe` tuyệt đối; không eval input.

## 7. Giao diện người dùng

- `tgdlrs-login.ps1` (WinForms): hiện khi chưa đăng nhập; lưu `api_id/api_hash/phone`;
  OTP/2FA vẫn gõ ẩn trong cửa sổ đen; tự phát hiện đăng nhập thành công.
- Shortcut `Desktop\Telegram Downloader - Login.lnk`.
- CLI `tgdlrs <cmd> [--json]`.

## 8. Lộ trình & trạng thái

| Pha | Nội dung | Trạng thái |
|---|---|---|
| P0 | Audit môi trường, toolchain, winsqlite3 | ✅ (GNU toolchain; SQLite 3.53.4; 123 GiB trống) |
| P1 | Core: sqlite FFI, models, store, DB migrations | ✅ + tests |
| P2 | Engine resume (plan, loop, retry, control) | ✅ + tests |
| P3 | Telegram connect/login/resolve/scan/search | ✅ compile; **chưa login thật** |
| P4 | Worker + named pipe + recovery + autostart | ✅ smoke-tested (spawn/ping/status/stop) |
| P5 | CLI (12 lệnh) + `--json` | ✅ |
| P6 | MCP 15 tools + structured outputs | ✅ live trong OpenCode session |
| P7 | GUI login + shortcut | ✅ test sandbox (config write/spawn/failure path) |
| P8 | **Tải CHINT.7z.021/022/024/025/026 + verify** | ⏳ chờ đăng nhập của chủ tài khoản |
| P9 | `7z t` khi đủ 26 phần | ⏳ phụ thuộc P8 |
| P10 | GitHub repo (private) + tài liệu | ✅ repo + docs trong quá trình; push sau khi verify |

## 9. Kiểm thử

- `cargo test --workspace` — **43 tests** xanh (core 36, MCP 7), gồm:
  resume cắt lùi chunk, pause/cancel/retry qua DB, round-trip pause→resume
  (bắt lỗi bind placeholder trong `transition`), verify jobs, missing parts,
  IPC round-trip, session snapshot round-trip/corrupt.
- `cargo clippy --workspace --all-targets -- -D warnings` — sạch.
- `cargo fmt --all -- --check` — sạch.
- `cargo build --release --workspace` — OK; binary chỉ phụ thuộc DLL hệ thống.
- Chưa cover được: **resume thật qua Telegram live** và **CDN redirect** —
  sẽ xác nhận khi đăng nhập và enqueue các phần CHINT.

## 10. Vận hành cho bộ CHINT (kế hoạch thực thi)

1. `tgdlrs login` (hoặc GUI) — một lần.
2. `telegram_scan_chat(-1001188194766)` xác nhận `CHINT.7z.021…026` có trong chat,
   lấy `message_id` từng phần (seed 1914 = `.001`; các phần khác ở message khác).
3. `telegram_find_missing_parts(D:\Downloads\Brave, CHINT.7z., 1..26)` —
   xác nhận thiếu `.021/.022/.024/.025/.026` (đã chạy: đúng 5 phần thiếu,
   `.023` đã có đủ 4.175.429.632 bytes).
4. `telegram_batch_download` enqueue đúng 5 link đó; **không** tạo job cho phần đã đủ.
5. Theo dõi `telegram_list_downloads` tới trạng thái `VERIFIED`.
6. `telegram_verify_file` lần cuối từng phần; tổng: 26/26 phần × 4.175.429.632 bytes = 108.561.170.432 bytes (~101,1 GiB).
7. `7z t CHINT.7z.001` — chỉ test toàn vẹn.
