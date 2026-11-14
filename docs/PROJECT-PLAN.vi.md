# Telegram Downloader RS — Kế hoạch chi tiết (tiếng Việt)

Tài liệu này mô tả **kế hoạch thiết kế, phân xử lý và trạng thái** của dự án,
phục vụ bảo trì và đánh giá. Trạng thái ghi nhận tại ngày **2026-11-14**.

---

## 1. Mục tiêu

1. Tải các file Telegram rất lớn (bộ `CHINT.7z.001…026` ~4,17 GB mỗi phần,
   tổng ~108,6 GB) vào `D:\Downloads\Brave`, **không tải lại** các phần đã đủ
   (`.001`–`.020` và `.023`, mỗi phần đúng 4.175.429.632 bytes).
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
| R7 | CLI 12 lệnh + `--json` | login/download/scan/status/list/pause/resume/cancel/retry/verify/doctor/daemon/mcp |
| R8 | MCP 15 tools, không block | `rmcp` tool_router; chỉ enqueue/controls |
| R9 | Không secrets trong chat/log/git | Session + creds chỉ ở `%APPDATA%\tgdlrs`; `.gitignore` |

Ràng buộc môi trường: không có MSVC linker / Windows SDK / CMake / libsqlite3 C
→ dùng GNU toolchain + MinGW-w64 local (`as`/`dlltool`); `cargo test` cần
`TMP`/`TEMP` trỏ tới thư mục ghi được (`target\tmp`).

## 3. Kiến trúc tổng thể

```
┌──────────────┐   DB writes (pause/resume/…)   ┌────────────────┐
│ tgdlrs CLI   │────────────────────────────────▶│                │
└──────┬───────┘                                 │  state.db      │
       │ IPC (named pipe, JSON-lines)              │  (winsqlite3,  │
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

- **Worker** là tiến trình duy nhất giữ session Telegram; CLI/MCP ghi ý định
  vào SQLite; worker tự thấy trong ~2 giây.
- **MCP** gọi Telegram chỉ qua worker (pipe); scan/search có timeout phía client.
- CLI `login` tạm dừng worker, chạy client tạm, lưu session rồi mở lại worker.

## 4. Mô hình dữ liệu & trạng thái

`%APPDATA%\tgdlrs\state.db` — các bảng: `downloads`, `download_parts`,
`download_events`, `telegram_sources`, `verification_results`, `settings`,
`schema_migrations`.

Vòng đời: `QUEUED → RESOLVING → DOWNLOADING → VERIFYING → COMPLETED → VERIFIED`
(nhánh lỗi: `RETRY_WAIT → QUEUED` hoặc `FAILED`; `PAUSED` từ hầu hết trạng thái;
`CANCELED`; verify lại có thể trả `FAILED`).

## 5. Thiết kế resume (điểm then chốt)

1. Mỗi job dùng `.part` = `<tên>.part` (hoặc `<tên>.<id8>.part` khi trùng).
2. `plan_resume(part_len, expected_size)`:
   - rỗng → từ 0;
   - chẵn 512 KiB → tiếp tục tại offset;
   - lẻ → cắt `.part` lùi về chunk chẵn gần nhất, tăng `redownloaded_bytes`;
   - `.part` lớn hơn expected → bỏ, ghi nhận.
3. Nguồn: `upload.getFile` offset chẵn 4 KiB; Grammers
   `iter_download().chunk_size(512 KiB).skip_chunks(offset / 512 KiB)`;
   offset lẻ qua `pending_skip` (tải chunk chứa offset rồi bỏ phần đầu).
4. Sau lỗi mạng: tạo lại `DownloadIter` tại offset hiện tại (iterator Grammers
   chết sau lỗi); backoff lũy thừa + jitter, trần 60 s; FloodWait tôn trọng
   đúng thời gian server yêu cầu.
5. Hoàn thành: kiểm tra `final_path` tồn tại → verify; khớp → rename no-overwrite;
   lệch → job FAILED, file tồn tại không đụng; `.part` giữ hoặc quarantine
   `<part>.bad-<ts>` khi verify sau hash thất bại.

### Giới hạn đã biết (trung thực)

- Nếu Telegram trả **CDN redirect**, Grammers (build `cdn_supported=false`) panic;
  chúng ta bắt panic và trả lỗi **không retryable** — không hỗ trợ CDN.
- FloodWait hàng giờ được tôn trọng (ưu tiên an toàn tài khoản).
- `skip_verified` bỏ tải lại, không bỏ verify-lại file đã tồn tại.

## 6. Bảo mật

- Named pipe: DACL `SY + user SID`; không TCP/LAN; payload JSON-lines, tối đa 1 MiB.
- Session/creds chỉ ở `%APPDATA%\tgdlrs`; GUI ghi `config.json` có backup;
  không log bí mật; MCP không trả secrets.
- Không shell ngoài; chỉ spawn `tgdlrs-worker.exe` tuyệt đối đường dẫn.

## 7. Giao diện người dùng

- `tgdlrs-login.ps1` (WinForms, song ngữ EN/VI): hiện khi chưa đăng nhập;
  lưu `api_id/api_hash/phone`; OTP/2FA vẫn gõ ẩn trong cửa sổ đen; tự phát hiện
  đăng nhập thành công qua exit code + session file; có nút **đăng nhập lại từ đầu
  (--force)** để đổi tài khoản; nhớ ngôn ngữ trong `config.json` (`gui_lang`).
- Shortcut `Desktop\Telegram Downloader - Login.lnk`.
- CLI `tgdlrs <cmd> [--json]`.

## 8. Lộ trình & trạng thái

| Pha | Nội dung | Trạng thái |
|---|---|---|
| P0 | Audit môi trường, toolchain, winsqlite3 | ✅ (GNU toolchain; SQLite 3.53.4; 123 GiB trống) |
| P1 | Core: sqlite FFI, models, store, migrations | ✅ + tests |
| P2 | Resume engine (plan/loop/retry/control) | ✅ + tests |
| P3 | Telegram connect/login/resolve/scan/search | ✅ compile; ✅ chạy live (account 玉玲珑) |
| P4 | Worker + named pipe + recovery + autostart | ✅ smoke-tested; ✅ autostart đăng ký |
| P5 | CLI 12 lệnh + `--json` | ✅ |
| P6 | MCP 15 tools + structured output | ✅ live trong OpenCode |
| P7 | GUI login + shortcut + đổi tài khoản --force | ✅ test (EN/VI, failure path) |
| P8 | **Tải CHINT.7z.021/022/024/025/026 + verify** | 🟡 **đang chạy** (5 job enqueued; .021 đang tải, ~364 MB (9%) lúc 14:12; ~2,8 GB/h) |
| P9 | `7z t` khi đủ 26 phần | ⏳ phụ thuộc P8 |
| P10 | GitHub repo công khai, song ngữ EN/VI | 🟡 đang làm (docs/GUI song ngữ; push **sau** khi P8/P9 xong) |

Ghi chú vận hành thực tế (2026-11-14):

- `omegaxz` (account đầu) **không** resolve được private channel → `CHANNEL_INVALID`.
  Chuyển sang account thành viên `玉玲珑` là hết lỗi. GUI có thêm nút
  "đăng nhập lại từ đầu (--force)" đúng cho tình huống này.
- 5 job placeholder tạo qua MCP URL-only trùng tên `download.bin`; đã cancel và
  re-enqueue với tên riêng từng phần; MCP đã được sửa: `placeholder_file_name`.
- Một `.part` 27,5 MB còn sót lại (`CHINT.7z.021.part`, chủ sở hữu là job
  placeholder đã cancel `1340664d`) được **giữ nguyên** theo đúng nguyên tắc.

## 9. Kiểm thử

- `cargo test --workspace` — **43 tests xanh** (36 core + 7 MCP).
- `cargo clippy --workspace --all-targets -- -D warnings` — sạch.
- `cargo fmt --all -- --check` — sạch.
- `cargo build --release --workspace` — OK; binary chỉ phụ thuộc DLL hệ thống.
- Resume thực tế qua chu kỳ `daemon stop` → `start`: ✅ (resume từ `.part`,
  không tải lại byte nào).
- Chưa automation: tải đầy đủ 5 phần live và hành vi CDN redirect (mục 5) —
  sẽ xác nhận sau P8.

## 10. Kế hoạch thực thi CHINT

1. `tgdlrs login` với account thành viên (`玉玲珑`) — **xong**.
2. `telegram_search_files(chat=-1001188194766, query="CHINT")` → tìm thấy
   `CHINT.7z.001…026` tại message id 1914…1939; các phần 4.175.429.632 bytes,
   `.026` = 1.415.085.203 bytes.
3. `telegram_find_missing_parts(1..26)` → thiếu đúng
   `.021 (msg 1934), .022 (1935), .024 (1937), .025 (1938), .026 (1939)`.
4. Enqueue đúng 5 link với tên rõ ràng.
5. `telegram_list_downloads` theo dõi tới `VERIFIED`.
6. `telegram_verify_file` lần cuối; rồi `7z t CHINT.7z.001` (chỉ test).
7. Push repo công khai lên GitHub sau P8/P9.
