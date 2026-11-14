# Telegram Downloader RS — Hướng dẫn sử dụng (tiếng Việt)

Tải file lớn (>4 GB) từ Telegram theo tính năng **resume thật**, chạy nền vĩnh viễn
qua worker riêng, điều khiển bằng CLI hoặc qua **MCP** trong OpenCode.

## 0. Yêu cầu

- Windows 10/11 x64.
- `api_id` / `api_hash` từ <https://my.telegram.org> (mục *API development tools*).
- Dung lượng ổ đĩa đủ (bộ CHINT.7z cần ~109 GB sau khi đủ 26 phần).
- Không cần quyền Administrator.

## 1. Cài đặt

```bat
C:\Users\OWG\AppData\Local\tgdlrs\bin\tgdlrs.exe doctor
```

`doctor` báo: config, thư mục tải `D:\Downloads\Brave`, dung lượng trống,
thư mục tải được ghi, SQLite 3.53.x, worker, session, autostart.

## 2. Đăng nhập một lần (bắt buộc trước khi tải)

Cách nhanh nhất — double-click **Telegram Downloader - Login** trên Desktop
(`%LOCALAPPDATA%\tgdlrs\tgdlrs-login.cmd`). Cửa sổ song ngữ hiện ra:

```
[ Trạng thái hiện tại ]   worker / session / tài khoản
[ Thông tin từ my...  ]   API ID + API Hash + SĐT  →  ① Lưu và gửi mã OTP
[ Thư mục tải         ]   chọn nơi lưu file (sửa hoặc duyệt)  →  Lưu thư mục tải
[ Hướng dẫn nhanh     ]   3 bước
```

1. Điền **API ID** (số), **API Hash** (32 hex, ẩn), **số điện thoại** `+84…`.
2. Bấm **① Lưu và gửi mã OTP** → mở cửa sổ đen `tgdlrs login`.
3. Trong cửa sổ đen: gõ mã OTP Telegram gửi vào app → Enter →
   mật khẩu 2FA (nếu có) → Enter. Các giá trị gõ **không hiện ra**.
4. Cửa sổ form chuyển xanh: **ĐĂNG NHẬP THÀNH CÔNG**.

### Đổi thư mục tải

Ngay trong cửa sổ trên, dùng mục **Thư mục tải**: sửa đường dẫn hoặc bấm **…**
để duyệt thư mục, rồi **Lưu thư mục tải**. Giá trị ghi vào `config.json`
(backup theo thời gian) và áp dụng cho **mọi job mới**. Job đã enqueue giữ
thư mục cũ trong row của nó; worker đang chạy giữ thư mục cũ tới khi restart
(`tgdlrs daemon restart` khi rảnh).

### Thông báo khi tải xong

Mỗi khi một job hoàn tất — **thành công hoặc thất bại** — một toast nhỏ hiện
góc dưới-phải (`tgdlrs-notify.ps1`, ngôn ngữ theo `gui_lang` trong `config.json`):

- `Tải xong: <file> — <dung lượng>, đã xác minh SHA-256`
- `Tải THẤT BẠI: <file> — <lý do>`

Cách terminal:

```bat
C:\Users\OWG\AppData\Local\tgdlrs\bin\tgdlrs.exe login
```

> Đổi tài khoản (khi account hiện tại không vào được channel): dùng
> **«Đăng nhập lại từ đầu (--force)»** trong GUI hoặc `tgdlrs login --force`.
> Session cũ tự backup thành `telegram.session.json.bak-<thời gian>`.
>
> Session lưu tại `%APPDATA%\tgdlrs\telegram.session.json`.
> **Không** chia sẻ/copy session; không gửi api_id/api_hash/OTP/2FA vào chat.

## 3. Tải file / bộ CHINT.7z

Enqueue một link:

```bat
tgdlrs download "https://t.me/c/1188194766/1914"
```

Worker nền (đã autostart cùng Windows) tự lấy hàng đợi, tải tuần tự
(`max_concurrent_files = 1`), resume `.part`, retry backoff + FloodWait,
SHA-256 khi xong, rồi mới đổi `.part` → tên thật.

Trong OpenCode (MCP `telegram-downloader`), chỉ cần nói ví dụ:

- «Quét chat `-1001188194766` xem có những phần CHINT.7z nào»
- «Enqueue các phần CHINT còn thiếu (không tải lại .001–.023)»
- «Cho xem tiến độ các download»
- «Verify các file CHINT đã tải»

Tôi (agent) sẽ gọi `telegram_scan_chat` / `telegram_search_files` /
`telegram_batch_download` / `telegram_list_downloads` / `telegram_verify_file`.

## 4. Theo dõi & điều khiển

```bat
tgdlrs status            rem worker + queue + tổng byte
tgdlrs list              rem danh sách job (id 8 ký tự đầu, state, %)
tgdlrs list --state RETRY_WAIT
tgdlrs status --json     rem dạng JSON
tgdlrs pause  <id>       rem worker tự thấy trong ~2 giây; .part được giữ
tgdlrs resume <id>
tgdlrs retry  <id>       rem FAILED/CANCELED → QUEUED, resume .part
tgdlrs cancel <id>       rem giữ .part, job thành CANCELED
tgdlrs verify <id>       rem size + SHA-256, ghi verification_results
tgdlrs verify "D:\Downloads\Brave\CHINT.7z.021" --path --expected-size 4175429632
```

- `<id>` = id đầy đủ hoặc tiền tố duy nhất.
- `.part` **không bao giờ bị xóa/nối mù**: đuôi không chẵn 512 KiB chunk bị
  cắt lùi về ranh giới chunk rồi tải lại phần đó (`redownloaded_bytes` ghi nhận).
- File đích đã tồn tại: chỉ đổi tên khi size + SHA-256 khớp; lệch → báo lỗi, **không ghi đè**.

## 5. Khi đủ 26 phần

```bat
"7z.exe" t D:\Downloads\Brave\CHINT.7z.001
```
(chỉ test, **không** giải nén). Cần 7-Zip (`E:\VScode_Opencode\Tools\mingw64\mingw64\bin\7z.exe`
hoặc bản cài 7-Zip chính thức).

## 6. Autostart & worker

- `tgdlrs daemon start` — chạy nền (không console), tự khôi phục job dở dang.
- `tgdlrs daemon stop` — dừng an toàn (đang tải sẽ kết thúc chunk rồi dừng, `.part` được giữ).
- `tgdlrs daemon status` — xem pid/version/authorized.
- `tgdlrs daemon install-autostart` — đăng ký `HKCU\...\Run\tgdlrs-worker` (đúng user, không cần admin).
- `tgdlrs daemon remove-autostart`

## 7. MCP trong OpenCode

MCP server tên **telegram-downloader** (`tgdlrs mcp`), 15 tools:
`telegram_login_status`, `telegram_resolve_link`, `telegram_scan_chat`,
`telegram_search_files`, `telegram_download`, `telegram_batch_download`,
`telegram_list_downloads`, `telegram_download_status`, `telegram_pause_download`,
`telegram_resume_download`, `telegram_cancel_download`, `telegram_retry_download`,
`telegram_verify_file`, `telegram_find_missing_parts`, `telegram_system_diagnostics`.

Nguyên tắc: tool chỉ enqueue/điều khiển; worker là tiến trình tải; không block
phiên chat; lỗi trả JSON `{"kind","message"}` (ví dụ `flood_wait`).

## 8. Xử lý sự cố

| Hiện tượng | Cách xử lý |
|---|---|
| `worker is not running` | `tgdlrs daemon start` |
| `telegram session is not authorized` | `tgdlrs login` |
| CHANNEL_INVALID | account hiện tại **không phải thành viên** channel — đăng nhập bằng account thành viên (GUI → Đăng nhập lại từ đầu, hoặc `tgdlrs login --force`) |
| `api_id is missing` | điền vào config.json hoặc biến môi trường User `TGDLRS_API_ID`/`TGDLRS_API_HASH` |
| FloodWait | chờ — worker tự ngủ đúng thời gian server yêu cầu, rồi chạy tiếp |
| File lỗi verify | job FAILED, `.part` quarantine thành `.part.bad-<ts>`; `tgdlrs retry <id>` để làm lại |
| Ổ đĩa dưới reserve 10 GiB | worker dừng cấp job mới, báo `disk_free_bytes` thấp trong `tgdlrs status` |
| `cargo test` lỗi Temp | trỏ `TMP`/`TEMP` vào `target\tmp` của workspace |

## 9. Bảo mật

- Session/`api_hash`/OTP/2FA không bao giờ ra khỏi máy, không vào log, không vào git.
- Named pipe `\\.\pipe\tgdlrs-<user>` chỉ user hiện tại + SYSTEM truy cập được
  (DACL tường minh). Không mở TCP port.
- `.gitignore` chặn `telegram.session.json`, `config.json`, `*.part`, `target/`.

## 10. Gỡ cài đặt

```bat
tgdlrs daemon stop
tgdlrs daemon remove-autostart
rmdir /s /q "%LOCALAPPDATA%\tgdlrs\bin"
rem session/config ở %APPDATA%\tgdlrs — xóa tay nếu muốn đăng xuất hẳn
```
Mở lại app sau cùng: gỡ entry `telegram-downloader` trong `opencode.json` nếu không dùng nữa.
