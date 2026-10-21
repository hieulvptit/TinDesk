# Cloudflare TURN: lấy token và cấu hình nhiều key

Backend giữ TURN key dài hạn và cấp ICE credential ngắn hạn cho phiên đã xác thực.
Client/agent chỉ nhận credential ngắn hạn. Backend dùng TTL 3.600 giây và refresh
trước hạn 300 giây; không đưa API token dài hạn xuống client.

## Lấy TURN Token ID và API Token

1. Mở [Cloudflare Realtime dashboard](https://dash.cloudflare.com/?to=/:account/calls),
   chọn account, vào phần TURN và tạo TURN key. Đặt tên dễ nhận biết, ví dụ
   `remotevnc-primary`.
2. Sao chép **TURN Token ID** và **API Token** của key vừa tạo vào file riêng.
   Nếu giao diện chỉ hiển thị secret một lần, lưu ngay bằng secret manager/editor.
3. Lặp lại để có key dự phòng. Mỗi cặp ID/token là một phần tử trong `keys` bên dưới.

[Tài liệu cấp credential của Cloudflare](https://developers.cloudflare.com/realtime/turn/generate-credentials/)
giải thích key dài hạn và API sinh credential. Nếu tạo key bằng
[API quản lý TURN](https://developers.cloudflare.com/api/resources/calls/subresources/turn/methods/create/),
management token cần quyền `Calls Write` trên account đã chọn; lấy `result.uid`
làm `key_id` và `result.key` làm `api_token`. Backend dùng token của TURN key;
không cần lưu management token dùng để tạo key.

## File cấu hình backend

Dùng file `/srv/remotevnc/secrets/turn-keys.json` ngoài repository:

```json
{
  "keys": [
    {
      "key_id": "TURN_TOKEN_ID_1",
      "api_token": "TURN_API_TOKEN_1"
    },
    {
      "key_id": "TURN_TOKEN_ID_2",
      "api_token": "TURN_API_TOKEN_2"
    }
  ]
}
```

Một key cũng hợp lệ. Giới hạn 16 key và 128 KiB; không trùng `key_id`. File phải
là file thường, không phải symlink, không cấp quyền group/other. Backend container
chạy UID/GID 10001 nên file cần thuộc UID 10001 và mode 0400 hoặc 0600:

```sh
sudo install -d -m0700 /srv/remotevnc/secrets
# Tạo/chỉnh file bằng editor hoặc secret manager, không đưa token vào command history.
sudo chown 10001:10001 /srv/remotevnc/secrets/turn-keys.json
sudo chmod 0600 /srv/remotevnc/secrets/turn-keys.json
```

Trong `/etc/remotevnc-backend/deploy.env`, chỉ ghi đường dẫn:

```dotenv
REMOTEVNC_TURN_KEYS_FILE=/srv/remotevnc/secrets/turn-keys.json
```

`packaging/backend/compose.yaml` mount file thành `/run/secrets/turn_keys.json`.
Không đặt thêm `REMOTEVNC_TURN_KEY_ID`, `REMOTEVNC_TURN_API_TOKEN` hoặc
`REMOTEVNC_TURN_API_TOKEN_FILE` trong môi trường container ở chế độ file.
Backend từ chối cấu hình lẫn hai chế độ, JSON sai, field lạ hoặc file không riêng tư.
Chạy binary trực tiếp cũng dùng `REMOTEVNC_TURN_KEYS_FILE` trỏ tới file riêng mà
UID chạy backend đọc được.

## Luân phiên, dự phòng và thay key

Mỗi lần cấp credential bắt đầu ở key kế tiếp. Nếu key lỗi mạng, hết thời gian,
trả lỗi HTTP hoặc ICE không hợp lệ, backend thử key kế tiếp; mỗi key tối đa một lần
trong lần cấp đó. Tổng thời gian của cả danh sách tối đa 5 giây. Chỉ trả bộ ICE
của một key thành công, không ghép credential từ nhiều key. Nếu tất cả thất bại,
TURN báo unavailable; hành vi kết nối trực tiếp hiện tại được giữ nguyên.

Backend đọc file lúc khởi động. Sau khi thêm/xóa/đổi key, giữ quyền file và tạo lại
riêng container backend để nạp file và cập nhật bind mount:

```sh
sudo docker compose --project-directory /etc/remotevnc-backend \
  --env-file /etc/remotevnc-backend/deploy.env \
  -f /etc/remotevnc-backend/compose.yaml up -d --force-recreate backend
```

Backend restart làm phiên cũ hết hiệu lực; client sẽ cần phiên mới. Khi rotate,
thêm key mới, nạp lại và kiểm tra trước khi bỏ key cũ. Token từng gửi trong chat
nên được rotate; các lần sau chỉ chia sẻ đường dẫn file riêng.

## Tương thích cấu hình một key cũ

Binary vẫn nhận cặp `REMOTEVNC_TURN_KEY_ID` và `REMOTEVNC_TURN_API_TOKEN`.
Entrypoint hỗ trợ đọc token từ `REMOTEVNC_TURN_API_TOKEN_FILE`. Với Compose,
dùng **riêng** `compose.legacy.yaml` làm file `-f`, cùng các biến
`REMOTEVNC_TURN_KEY_ID` và `REMOTEVNC_TURN_TOKEN_FILE` trong deploy.env. Không layer
hai Compose file. Chế độ file JSON phía trên là mặc định cho deployment mới.

## Kiểm tra

Cloudflare trả HTTP 201 khi cấp credential thành công. Kiểm tra trạng thái và số
ICE URL; không in token, username hay credential vào log. HTTP 401/403 thường cần
kiểm tra lại đúng cặp ID/token hoặc trạng thái key. Khi kiểm tra relay, phải quan
sát candidate được chọn là `relay`; API cấp credential thành công chưa chứng minh
đường truyền TURN thực tế.
