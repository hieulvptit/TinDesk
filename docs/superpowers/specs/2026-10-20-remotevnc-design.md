# RemoteVNC cho macOS và Ubuntu 26.04

Trạng thái: người dùng đã duyệt ngày 2026-10-20 bằng yêu cầu “ok triển đi”.

## 1. Mục tiêu và quyết định đã chốt

RemoteVNC cho phép máy macOS điều khiển đúng màn hình vật lý của Ubuntu 26.04, gồm màn hình đăng nhập, desktop và màn hình khóa. Hai thiết bị phải được đăng ký bằng cùng tài khoản Google trong ứng dụng. Việc Ubuntu chưa đăng nhập hệ điều hành không ngăn agent xuất hiện online hoặc nhận kết nối.

Người dùng đã chọn phương án B, điều khiển màn hình vật lý, và chấp nhận X11. Bản đầu đề xuất Xfce, LightDM với greeter X11 và trình khóa màn hình tương thích X11. GNOME trên Ubuntu 26.04 chỉ hỗ trợ phiên Wayland; XWayland không thay thế một phiên Xorg để điều khiển toàn bộ màn hình.

Ưu tiên độ trễ thao tác chuột và bàn phím, nhập UTF-8/tiếng Việt, kết nối P2P và tự dùng Cloudflare TURN khi không kết nối trực tiếp được. Agent là dịch vụ hệ thống tự chạy từ lúc boot. Client macOS có tùy chọn mở cùng phiên đăng nhập macOS.

## 2. Phạm vi bản đầu

- Một màn hình vật lý được chọn trên `seat0`, một client điều khiển tại một thời điểm.
- Client giao diện macOS, agent Ubuntu và backend quản lý đều viết bằng Rust. Chấp nhận thư viện native như GStreamer và X11; đây không phải yêu cầu toàn bộ dependency phải thuần Rust.
- Mac dùng IME cục bộ để nhập tiếng Việt; chuỗi đã được IME xác nhận được gửi dưới dạng UTF-8.
- Điều khiển chuột, click, kéo thả, cuộn, phím chức năng, tổ hợp phím và văn bản.
- Màn hình login/khóa vẫn sử dụng xác thực của Ubuntu. Tài khoản Google xác định quyền kết nối thiết bị, không tự đăng nhập hoặc bỏ qua mật khẩu Ubuntu.
- Agent và danh tính thiết bị tồn tại qua reboot; không cần mở trình duyệt đăng nhập Google sau mỗi lần boot.
- Không đưa âm thanh, truyền file, đồng bộ clipboard tổng quát, nhiều màn hình đồng thời, desktop ảo hoặc Wayland vào bản đầu.
- Hỗ trợ bắt đầu từ lúc Ubuntu đã khởi động hệ điều hành, có mạng và display manager. Màn hình firmware, bootloader và mở khóa mã hóa ổ đĩa trước khi hệ điều hành chạy nằm ngoài phạm vi.

Ubuntu amd64 là đích build đầu tiên; cấu trúc mã không cố định kiến trúc CPU. Bản macOS được build cho kiến trúc của máy phát triển, có hướng dẫn build cho Apple Silicon và Intel. Chưa có địa chỉ SSH hoặc thông tin GPU của Ubuntu, nên kiểm thử phần cứng là bước nghiệm thu riêng.

## 3. Kiến trúc

```text
Client macOS ── HTTPS/WSS ── Backend Rust ── WSS ── Agent Ubuntu
      │                                               │
      └──── WebRTC P2P hoặc Cloudflare TURN ────────────┘
                                                      │ Unix socket
                                                X11 seat helper
                                                      │
                                           LightDM / desktop / locker
```

### Backend quản lý

Một dịch vụ Rust dùng Axum/Tokio và SQLite cho triển khai một instance. Backend xử lý đăng nhập Google, đăng ký và thu hồi thiết bị, trạng thái online, cấp phiên điều khiển, signaling WebRTC và cấp credential Cloudflare TURN có thời hạn. Khóa OAuth và Cloudflare chỉ nằm ở backend.

Backend chạy trên máy có HTTPS/WSS truy cập được từ cả hai thiết bị; có thể dùng VPS nhỏ hoặc reverse proxy/Cloudflare Tunnel trước dịch vụ Rust. Cloudflare Tunnel dùng cho API và signaling. Video và input đi qua WebRTC, trực tiếp hoặc qua Cloudflare TURN.

### Agent Ubuntu

Agent kết nối outbound tới backend, dùng tài khoản hệ thống riêng và tự khởi động qua systemd. Nó xác thực phiên điều khiển, quản lý WebRTC và chuyển input đã kiểm tra tới helper. Agent không yêu cầu phiên desktop người dùng, không giữ mật khẩu Ubuntu và không mở cổng remote desktop công khai.

### X11 seat helper

Helper là tiến trình cục bộ có quyền truy cập X server và Xauthority của LightDM/phiên đang hoạt động. Nó theo dõi `seat0`, chụp màn hình, mã hóa video và gửi input qua XTest. Helper không kết nối Internet; giao tiếp qua Unix socket chỉ dành cho UID agent, kiểm tra thông tin tiến trình kết nối và giới hạn loại/kích thước yêu cầu.

Helper cung cấp các thao tác cố định: trạng thái màn hình, bắt đầu/dừng stream, đổi cấu hình chất lượng trong giới hạn, yêu cầu keyframe, input và nhả các phím đang giữ. Không cung cấp shell hoặc đọc đường dẫn tùy ý. GStreamer mã hóa trong worker của helper và chuyển các access unit video có timestamp qua IPC tới agent; agent đưa vào pipeline RTP/WebRTC.

Bổ sung kỹ thuật sau kiểm chứng native ngày 2026-10-21: XTest và keymap có thể tồn tại sau khi helper bị SIGKILL. Một input guardian chạy trong dịch vụ/cgroup riêng, chỉ dùng IPC cục bộ dành cho root, giữ trạng thái phục hồi trong RAM và kết nối X11 độc lập gắn với đúng X server. Helper ghi nhận trạng thái và chờ xác nhận trước khi sửa input/keymap; guardian phục hồi khi helper chết, còn helper giữ trách nhiệm phục hồi nếu guardian chết. Khi hết deadline hoặc IPC phục hồi mất bất thường, guardian dùng SO_PEERPIDFD để dừng đúng tiến trình helper rồi đợi kết nối X11 của helper kết thúc trước khi hoàn tất phục hồi; systemd khởi động lại helper. Cần Linux kernel từ 6.5, không nhận PID tùy ý từ giao thức remote. Không ghi journal nội dung nhập lên đĩa. Phím/nút đang đồng thời được thiết bị vật lý giữ phải chờ người dùng thả trước khi dọn phần input từ remote. Cả hai dùng deadline tuyệt đối CLOCK_BOOTTIME tối đa 60 giây, không tự gia hạn từ hoạt động display/input. Việc đồng thời mất cả helper và guardian khi X server vẫn sống là một lỗi kép riêng, phải nêu rõ giới hạn nghiệm thu. XTest không gộp truy vấn trạng thái nguồn vật lý và nhả input thành một thao tác nguyên tử: phím/nút vừa được người dùng nhấn giữa hai thao tác đó vẫn có thể bị ảnh hưởng. Việc kiểm tra rồi phục hồi có điều kiện đối với keymap, lock và group cũng có thể ghi đè thay đổi X11 cục bộ xuất hiện giữa hai thao tác. Phải ghi rõ giới hạn khi điều khiển tại máy và từ xa cùng lúc trong khoảng phục hồi này; không tuyên bố loại bỏ hoàn toàn race hoặc có cận thời gian cứng.

### Client macOS

Giao diện Rust dùng eframe/egui, tích hợp sự kiện IME của cửa sổ, bộ nhận GStreamer/WebRTC và frame mới nhất để hiển thị. UI gồm đăng nhập, danh sách Ubuntu online, trạng thái phiên và cửa sổ remote. Hiển thị kết nối trực tiếp hay relay, độ phân giải, FPS và thông tin độ trễ lấy được từ WebRTC.

## 4. Đăng nhập Google và quyền thiết bị

Backend dùng OpenID Connect authorization code với state, nonce và PKCE. Xác thực chữ ký ID token, issuer, audience, thời hạn và nonce bằng thư viện chuẩn. Google `sub` là khóa định danh chủ sở hữu; không dùng email do client tự gửi để quyết định quyền.

Mỗi ứng dụng tạo cặp khóa thiết bị và bắt đầu một giao dịch đăng ký có thời hạn. Giao dịch gắn với khóa công khai, verifier bí mật và mã xác nhận hiển thị trên ứng dụng. Người dùng mở trình duyệt, đăng nhập Google và xác nhận tên thiết bị/mã đăng ký. Chỉ thiết bị chứng minh giữ verifier và khóa riêng mới nhận credential ứng dụng. URL trình duyệt không chứa credential dài hạn.

Ubuntu đăng ký một lần bằng lệnh quản trị. Có thể mở URL xác nhận trên macOS, nên không cần trình duyệt trong màn hình login Ubuntu. Ubuntu lưu khóa dưới thư mục state của dịch vụ với quyền truy cập riêng; macOS lưu trong Keychain. Token Google không được dùng làm credential remote dài hạn; backend cấp credential thiết bị có thể thu hồi và access token ngắn hạn sau khi xác thực thiết bị.

Mọi API danh sách thiết bị, signaling, tạo phiên, cấp TURN và thu hồi đều kiểm tra chủ sở hữu ở backend. Thiết bị khác tài khoản bị từ chối kể cả biết device ID hoặc session ID. Một agent chỉ có một lease điều khiển; yêu cầu thứ hai nhận trạng thái đang bận. Thu hồi thiết bị ngăn tạo phiên và đóng phiên đang chạy.

Agent làm mới lease quyền điều khiển mỗi 15 giây; không gia hạn được trong 60 giây thì dừng stream và nhả toàn bộ input. Policy này giới hạn thời gian duy trì một phiên không còn xác minh được quyền sau khi mất backend.

## 5. Kết nối và ưu tiên độ trễ

Backend xác thực cả hai thiết bị trước khi cho trao đổi offer, answer và ICE candidate. WebRTC dùng trickle ICE, thử đường trực tiếp và relay theo kết quả ICE. Cung cấp tùy chọn ép relay để kiểm thử và chẩn đoán. Hai phía xác minh peer/session theo phiên backend đã cấp; không nhận offer hoặc input ngoài phiên.

Cloudflare TURN được cấp credential ngắn hạn theo phiên đã được cho phép; tự làm mới trước khi hết hạn nếu phiên còn hợp lệ và thực hiện ICE restart khi cần. Không nhúng khóa API Cloudflare vào app. Đường relay có thể dùng UDP hoặc TURN qua TCP/TLS khi môi trường mạng yêu cầu.

Video dùng GStreamer `webrtcbin` và H.264 low latency, ưu tiên encoder phần cứng khi có và tương thích hai phía; dùng encoder phần mềm khi cần. Encoder không dùng B frame; hàng đợi hữu hạn, loại frame cũ trước mã hóa khi nghẽn và yêu cầu keyframe sau thay đổi display/khôi phục kết nối. Profile mặc định là tối đa 1080p60, giảm FPS/độ phân giải/bitrate khi không đủ tài nguyên hoặc băng thông. Đây là cấu hình mục tiêu, không phải cam kết FPS trên GPU chưa kiểm thử.

Input tách hai data channel:

- Chuyển động chuột: unordered, không retransmit, có sequence và tọa độ tuyệt đối; chỉ áp dụng vị trí mới nhất.
- Phím, nút chuột, cuộn và văn bản: reliable, ordered. Sự kiện click mang tọa độ của nó để không phụ thuộc thứ tự giữa hai channel.

Mỗi thông điệp chứa phiên bản protocol, session ID và display generation. Từ chối input cho display generation cũ để không bắn nhầm khi chuyển login/desktop. Giới hạn kích thước văn bản và tốc độ thông điệp; không để dữ liệu video chặn xử lý input.

Khi mất kết nối hoặc cửa sổ remote mất focus, nhả phím/nút đang giữ. Kết nối lại tạo phiên input sạch và khôi phục video từ keyframe; không phát lại các thao tác cũ.

## 6. Màn hình vật lý và Unicode

Helper tìm display và Xauthority của X server đang hoạt động qua LightDM/logind, không giả định cố định `DISPLAY=:0` hoặc `~/.Xauthority`. Chỉ chọn display thuộc seat vật lý; không tạo Xvfb hoặc phiên đăng nhập remote để thay thế yêu cầu B.

Các trạng thái gồm chờ display manager, login, desktop, locked và đổi display. Khi X server thay đổi hoặc reset, helper dừng capture/input cũ, nhả phím còn giữ nếu kết nối X còn sống, đọc lại authority, tăng generation, kết nối X mới và khởi động lại stream. Client hiển thị đang đổi màn hình trong khoảng chuyển tiếp, sau đó tiếp tục điều khiển cùng phiên đã được cho phép.

Các phím vật lý/tổ hợp được ánh xạ theo XKB và trạng thái modifier của Ubuntu. IME macOS chỉ gửi text sau commit để tránh nhập hai lần. Helper chèn Unicode keysym bằng XTest; ký tự chưa có trong keymap dùng keycode trống với mapping tạm thời, được serialize và phục hồi khi hoàn thành/đóng phiên. Không dùng clipboard mặc định để hỗ trợ cả trường mật khẩu và tránh làm thay đổi clipboard người dùng.

Kiểm thử phải xác nhận tiếng Việt có dấu, chuỗi NFC/NFD, CJK, ký tự ngoài BMP và tổ hợp phím trên greeter, locker, trình soạn thảo và terminal. XTest không tự bảo đảm mọi ứng dụng X11 nhận mọi Unicode keysym giống nhau; ứng dụng không hỗ trợ phải báo giới hạn rõ, không được tuyên bố hỗ trợ chỉ vì packet UTF-8 truyền thành công. Đường nhập Unicode và chuyển greeter/desktop là hai hạng mục kiểm chứng đầu tiên trước khi hoàn thiện UI.

## 7. Cài đặt và vận hành

Gói Ubuntu gồm binaries, systemd units, cấu hình mẫu và công cụ kiểm tra môi trường. Installer kiểm tra Xorg, Xfce, LightDM, XTest, encoder và quyền Xauthority; cấu hình greeter, desktop và khóa màn hình cùng dùng X11. Đăng ký agent với Google hoàn tất trước khi reboot thử nghiệm.

Chuyển display manager là thay đổi cấp máy, cần quyền quản trị và có thể kết thúc desktop đang dùng. Công cụ cài đặt cung cấp bước chuẩn bị và bước áp dụng tường minh, sao lưu cấu hình, hướng dẫn rollback qua SSH/TTY; không âm thầm đổi display manager trong một lệnh build hoặc test.

Client macOS có hướng dẫn cài GStreamer và build/chạy. Client chỉ nhận video và lấy input trong cửa sổ của chính nó, nên không xin Screen Recording hoặc quyền điều khiển toàn hệ thống macOS nếu không có tính năng cần quyền đó. Tùy chọn mở cùng đăng nhập macOS phải được người dùng bật.

Backend có cấu hình Google OAuth, public origin, SQLite, signing key và Cloudflare TURN bằng biến môi trường/file riêng. Repository chỉ chứa mẫu không có secret. Log ghi vòng đời phiên và lỗi, không ghi văn bản đã nhập, mật khẩu, token, private key hoặc frame màn hình.

Chưa có thông tin VPS/domain, OAuth client và Cloudflare TURN key. Các adapter và hướng dẫn cấu hình phải hoàn thiện trong repo; triển khai live và nghiệm thu Google/TURN dùng credential thật khi người dùng cung cấp. Mock chỉ dành cho test, không có chế độ bỏ xác thực trong cấu hình production.

## 8. Nghiệm thu

1. Boot Ubuntu tới greeter, chưa đăng nhập hệ điều hành: agent online, macOS cùng tài khoản Google thấy đúng màn hình vật lý, nhập thông tin đăng nhập và điều khiển desktop sau login.
2. Lock/unlock, logout/login và khởi động lại LightDM: hiển thị đúng màn hình vật lý, tự nối lại capture khi cần, không kẹt phím/nút chuột, không tạo phiên desktop khác.
3. Tài khoản Google khác không liệt kê/điều khiển thiết bị; thử gọi thẳng API và gửi signaling trái phép cũng bị từ chối.
4. Phiên dùng P2P thật trên mạng cho phép; chế độ relay-only dùng credential Cloudflare thật và thống kê xác nhận selected candidate là relay.
5. Xác nhận Unicode ở các ứng dụng và màn hình nêu ở mục 6, gồm mật khẩu thử nghiệm Unicode. Không log dữ liệu nhập.
6. Mất mạng, backend restart, credential hết hạn, thu hồi thiết bị và helper crash: chuyển trạng thái rõ ràng, giải phóng input, tuân thủ lease và tự kết nối lại khi hợp lệ.
7. Đo FPS, RTT, thời gian hàng đợi input/capture và độ trễ thao tác tới hình ảnh trên LAN và TURN. Mục tiêu kỹ thuật LAN là p95 thao tác tới hình ảnh dưới 100 ms ở 1080p trên phần cứng thử đủ năng lực; báo kết quả đo kèm cấu hình GPU/CPU/mạng. Không suy ra độ trễ này chỉ từ RTT.

Unit/integration test bao phủ xác thực, phân quyền, phiên, protocol, input và vòng đời dịch vụ. Xvfb/Xephyr dùng để tự động kiểm thử X11 nhưng không thay thế nghiệm thu boot/login/lock trên máy Ubuntu với màn hình vật lý. Build macOS và Ubuntu phải được xác nhận riêng; không coi build trên macOS là bằng chứng Linux chạy được.

## 9. Nguồn kỹ thuật

- [Ubuntu 26.04 và phiên GNOME Wayland](https://documentation.ubuntu.com/release-notes/26.04/summary-for-lts-users/).
- [LightDM và cấu hình display manager](https://github.com/ubuntu/lightdm/blob/main/data/lightdm.conf).
- [x11vnc và truy cập màn hình X vật lý](https://github.com/LibVNC/x11vnc): tham khảo hành vi và kiểm thử, không bắt buộc nhúng x11vnc.
- [GStreamer ximagesrc](https://gstreamer.freedesktop.org/documentation/ximagesrc/index.html) và [webrtcbin](https://gstreamer.freedesktop.org/documentation/webrtc/).
- [Google OpenID Connect](https://developers.google.com/identity/openid-connect/openid-connect).
- [Cloudflare TURN credential](https://developers.cloudflare.com/realtime/turn/generate-credentials/).
