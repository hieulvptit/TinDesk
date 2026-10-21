# Backend amio

Deployment host: `ubuntu@103.67.184.135` (Ubuntu 24.04, amd64).
Public origin: `https://amio.vnoffice.io.vn`.
Google callback: `https://amio.vnoffice.io.vn/login/callback`.

## Cấu hình đang dùng

- `/etc/remotevnc-backend/compose.yaml`: một backend, tự chạy lại cùng Docker.
- `/etc/remotevnc-backend/deploy.env`: origin, client ID và đường dẫn file; không chứa token.
- `/srv/remotevnc/secrets/google-client-secret`: Google client secret dạng raw.
- `/srv/remotevnc/secrets/turn-keys.json`: danh sách TURN key, hiện có một key.
- `/srv/remotevnc/data`: SQLite và backup riêng tư, UID/GID 10001, mode 0700.

Hai file secret thuộc UID/GID 10001, mode 0600, nằm ngoài repository.
Xem [hướng dẫn lấy và thêm nhiều TURN key](cloudflare-turn.md).

Nginx chỉ thêm vhost `amio` và các snippet có tiền tố `remotevnc`, proxy API tới
`127.0.0.1:18080`. Chế độ SSL Cloudflare **giữ nguyên theo yêu cầu của chủ hệ thống**;
không tạo Configuration Rule hay đổi sang Full (strict). Vhost origin nhận cả HTTP
và HTTPS, không redirect HTTP để tránh vòng lặp với chế độ origin hiện tại.
Origin có chứng chỉ Let's Encrypt riêng cho `amio`; timer có sẵn
`snap.certbot.renew.timer` gia hạn và hook riêng chỉ reload nginx khi cert `amio` đổi.

`/` trả dòng chữ từ nginx để nhận diện route. Dòng chữ này không phải health check
của tiến trình backend. Kiểm tra `/v1/devices` không có token phải nhận HTTP 401.

## Vận hành

```sh
# Trên server
sudo docker compose --project-directory /etc/remotevnc-backend \
  --env-file /etc/remotevnc-backend/deploy.env \
  -f /etc/remotevnc-backend/compose.yaml ps

# Nạp lại file TURN sau khi sửa/thay file, giữ nguyên quyền 0600 và UID 10001
sudo docker compose --project-directory /etc/remotevnc-backend \
  --env-file /etc/remotevnc-backend/deploy.env \
  -f /etc/remotevnc-backend/compose.yaml up -d --no-build --force-recreate backend

# Chỉ kiểm tra status, không in token/response body
curl -A RemoteVNC-deploy/0.1 -s -o /dev/null -w '%{http_code}\n' \
  https://amio.vnoffice.io.vn/v1/devices

sudo systemctl status remotevnc-maintenance.timer --no-pager
```

Backend đọc key lúc khởi động; restart làm hết hiệu lực các phiên cũ. Timer
`remotevnc-maintenance.timer` chạy mỗi giờ, chỉ dọn bản ghi tạm đã hết hạn;
không xóa thiết bị, người dùng hoặc lịch sử thu hồi khóa. Backup/restore dùng
quy trình SQLite Online Backup API trong [hướng dẫn backend](backend.md).

Để thay image, giữ lại image ID cũ rồi trỏ tag `remotevnc-backend:local` tới image
đã kiểm tra và chạy lại Compose ở trên. Rollback image không thay hoặc khôi phục
database cũ. Muốn tắt riêng backend, dùng cùng Compose arguments với `stop backend`;
không dừng nginx/Docker hay các ứng dụng khác trên host.

## Bản đã triển khai ngày 2026-10-21

- Binary source: `86709e8042c19f3925113d2a442f9cf7ea5f3713`.
- Compose: `0c85c89a2ffc029a556cb6b0a818de21bb1fc6e9`, sửa mount TURN thành
  `/run/secrets/turn_keys.json` trước khi nghiệm thu.
- Image tag: `remotevnc-backend:86709e8` (cùng image với tag `local`).
- Immutable image ID:
  `sha256:c5cd5b2df20ade8389197e98e05b0253ecac9402bf1ab09dd3b5b478bab173b6`.
- Dockerfile dùng cho build offline, SHA-256:
  `0c72b9744842b71f909927a0500a7371502e8ee971c97495b17941bf0b61eed3`.

Image build trên server từ `git archive` của source commit trên. Registry có nhiều
timeout, nên recipe thực tế thêm public crates.io cache vào builder rồi chạy
`cargo build --locked --offline --release -p remotevnc-backend -j 1`. Rust, base
image, Debian snapshot và Cargo.lock giữ nguyên; Cargo kiểm tra checksum crate.
Cache chỉ nằm trong builder, không nằm trong runtime image. Source archive, recipe,
log và bằng chứng kiểm tra được lưu riêng tại thư mục local
`/Users/hieulv/Projects/RemoteVNC-builds/backend-20261021-86709e8`.
Server lưu manifest không chứa secret tại `/etc/remotevnc-backend/image-record.json`.

Kiểm tra đã đạt:

- 51 backend test; fmt, Clippy và kiểm tra entrypoint trước khi đóng gói.
- Build release Linux amd64 và Docker image exit 0.
- Container UID 10001, read-only root, bỏ capability, no-new-privileges,
  chỉ publish loopback; file secret/SQLite riêng tư. Container sau khi sửa mount
  chạy với restart count 0 lúc nghiệm thu.
- Public HTTPS: landing 200; API và WebSocket chưa đăng nhập 401, `no-store`.
- Tạo một enrollment tạm; Google redirect 303, callback đúng `amio`, có state,
  nonce và S256 PKCE. Không đi tiếp trang đồng ý của Google.
- Fake callback bị từ chối; marker trong query/header không xuất hiện trong
  access log riêng của nginx. Kiểm tra này không bao gồm log bên Cloudflare.
- Cloudflare cấp credential HTTP 201 từ runtime container với key đã cấu hình.
- TLS trực tiếp origin hợp lệ; `nginx -t` đạt. Chế độ SSL Cloudflare giữ nguyên.
- Maintenance oneshot thành công, timer mỗi giờ đã bật; bootstrap backup
  `/srv/remotevnc/data/bootstrap-20261021.db` tạo bằng Online Backup API,
  integrity check đạt, UID 10001/mode 0600.

Sau đó đã xác minh Google consent/token exchange và đăng ký agent thực tế trên
`clawmachine@172.22.8.59`: người dùng xác nhận thiết bị, backend đăng ký và cấp
access token cho agent. Xem [bản ghi triển khai agent](deployment-clawmachine.md).

Chưa nghiệm thu riêng kết nối WebSocket đã đăng nhập và phiên remote thực tế có
candidate `relay`. HTTP 201 của provider chưa chứng minh media đã truyền qua TURN.
Nghiệm thu màn hình login/khóa trên máy Ubuntu vật lý vẫn là bước riêng.
