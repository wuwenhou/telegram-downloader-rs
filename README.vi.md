# Telegram Downloader RS

🇻🇳 Tiếng Việt · 🇬🇧 [English](README.md)

Tải file Telegram rất lớn với **resume thật ở mức byte**, chạy nền bền vững
cho Windows x64 — ví dụ bộ nén nhiều phần `CHINT.7z.001…026`.

- Core engine bằng Rust (MTProto qua Grammers, SQLite qua `winsqlite3.dll`, không cần toolchain C)
- `tgdlrs-worker` chạy nền độc lập OpenCode; tự khôi phục job dở dang sau restart
- CLI `tgdlrs`, giao diện đăng nhập song ngữ và MCP server `telegram-downloader`
- Resume thật theo chunk 512 KiB căn chỉnh từ `upload.getFile` của Telegram
- Hướng dẫn: [docs/USER-GUIDE.vi.md](docs/USER-GUIDE.vi.md) · [english](docs/USER-GUIDE.en.md)
- Kế hoạch: [docs/PROJECT-PLAN.vi.md](docs/PROJECT-PLAN.vi.md) · [english](docs/PROJECT-PLAN.en.md)

## Các file nhị phân

| Binary | Chức năng |
| --- | --- |
| `tgdlrs.exe` | CLI; cũng chạy MCP server với `tgdlrs mcp` |
| `tgdlrs-worker.exe` | Worker nền; giữ session Telegram |
| `tgdlrs-mcp.exe` | MCP server (giống `tgdlrs mcp`) |

Cài tại `%LOCALAPPDATA%\tgdlrs\bin\`; tự chứa (chỉ phụ thuộc DLL hệ thống;
SQLite được nạp từ `winsqlite3.dll` lúc chạy).

## Cài đặt lần đầu

1. Lấy `api_id`/`api_hash` tại <https://my.telegram.org> (*API development tools*).
2. Hoặc chạy GUI đăng nhập song ngữ:

   ```bat
   powershell -NoProfile -ExecutionPolicy Bypass -File extras\install-login-gui.ps1
   ```

   rồi double-click **Telegram Downloader - Login** trên Desktop — hoặc điền
   thông tin trực tiếp vào `%APPDATA%\tgdlrs\config.json` (tự tạo lần chạy đầu)
   hay đặt biến môi trường `TGDLRS_API_ID` / `TGDLRS_API_HASH`.
3. Đăng nhập một lần. GUI hỏi số điện thoại, mã OTP Telegram gửi vào app và
   mật khẩu 2FA của bạn (nếu có). Các giá trị này cũng dùng được với
   `tgdlrs login` trong terminal. **Không bao giờ dán chúng vào chat.**
4. `tgdlrs daemon install-autostart` để worker tự chạy khi đăng nhập Windows.

## Sử dụng

```powershell
tgdlrs download "https://t.me/c/1188194766/1914"   # enqueue; worker tự tải tiếp
tgdlrs status                                       # worker + hàng đợi
tgdlrs list [--state RETRY_WAIT] [--limit 20]
tgdlrs pause <id> | resume <id> | retry <id> | cancel <id>
tgdlrs verify <id|path>                             # size + SHA-256
tgdlrs scan -1001188194766                          # liệt kê document trong chat
tgdlrs doctor                                       # chẩn đoán
tgdlrs daemon start|stop|status|install-autostart
tgdlrs --json <command>                             # output máy đọc được
```

`<id>` là id đầy đủ hoặc tiền tố duy nhất. `pause` được worker nhận trong vài
giây; `.part` không bao giờ bị xóa hay nối mù, chỉ đổi tên thành file cuối sau
khi size + SHA-256 khớp. Khi restart, phần đuôi không chẵn chunk 512 KiB bị cắt
lùi về ranh giới chunk và tải lại.

## MCP

`tgdlrs mcp` nói chuyện MCP qua stdio. Server **telegram-downloader** đăng ký
trong `opencode.json` tại `mcp.servers["telegram-downloader"]`. 15 tools
`telegram_*` (xem `docs/USER-GUIDE.vi.md` phần 7) — chỉ enqueue/điều khiển,
không block phiên chat; lỗi trả JSON `{"kind","message"}` (ví dụ `flood_wait`).

## GUI đăng nhập (`extras/`)

- `tgdlrs-login.ps1` / `tgdlrs-login.cmd` — cửa sổ đăng nhập song ngữ EN/VI,
  cài bằng `extras/install-login-gui.ps1`.
- OTP/2FA vẫn gõ ẩn trong cửa sổ đen; chỉ `api_id`/`api_hash`/phone được lưu,
  ở `%APPDATA%\tgdlrs\config.json` (tự backup theo thời gian khi lưu).

## Resume semantics (trung thực)

- Chỉ phần đã hoàn chỉnh chẵn 512 KiB của `.part` được tin tưởng. Đuôi lẻ được
  tải lại; `redownloaded_bytes` ghi nhận chi phí.
- Nếu Telegram trả **CDN redirect**, Grammers panic ở build này; panic bị bắt và
  báo lỗi **không retryable** rõ ràng — không hỗ trợ CDN.
- Retry dùng backoff lũy thừa + jitter; tôn trọng `FLOOD_WAIT`; lỗi mạng retry
  tối đa `max_attempts`; lỗi không-retryable fail job.
- `SKIP_VERIFIED` không bao giờ ghi đè file cuối; lệch hash là lỗi xung đột cứng.

## Build & test

```powershell
cargo build --release --workspace
cargo test --workspace     # đặt TMP/TEMP vào thư mục ghi được (vd. target\tmp)
cargo clippy --workspace --all-targets -- -D warnings
```

Workspace dùng `stable-x86_64-pc-windows-gnu` (xem `rust-toolchain.toml`).
GNU toolchain cần `as`/`dlltool` trên `PATH`; máy này dùng MinGW-w64 tại
`E:\VScode_Opencode\Tools\mingw64\mingw64\bin`. `cargo test` cần thư mục temp
ghi được; trỏ `TMP`/`TEMP` vào `target\tmp` nếu thư mục temp mặc định không ghi được.

## Bố cục repo

- `crates/tgdlrs-core` – FFI SQLite, models, engine resume, IPC, logic worker
- `crates/tgdlrs-cli` – binary `tgdlrs`
- `crates/tgdlrs-worker` – binary worker nền
- `crates/tgdlrs-mcp` – MCP server (library + binary `tgdlrs-mcp`)
- `extras/` – GUI đăng nhập song ngữ + installer
- `docs/` – hướng dẫn & kế hoạch (EN/VI)

Thư mục tải mặc định: `D:\Downloads\Brave` (đổi được trong
`%APPDATA%\tgdlrs\config.json`). Không bao giờ ghi đè file tồn tại khi chưa khớp
identity; `.part` được giữ lại khi hỏng.

## Giấy phép

MIT
