# Private single-instance backend deployment

The backend is a Rust service with SQLite and no GStreamer dependency. Run exactly
one instance: authorization serialization, socket ownership/presence and session
reconciliation are process-local. Do not scale replicas or mount SQLite on a
network filesystem. HTTPS ingress is required; only loopback port18080 is published.
The container runs as UID/GID 10001 with private state, read-only root filesystem,
no capabilities and no exposed public backend port.

`https://amio.vnoffice.io.vn/login/callback` is the registered Google callback.
The owner confirmed that the Cloudflare-proxied `amio.vnoffice.io.vn` domain is
**dedicated to RemoteVNC**. TURN supports a private JSON key list; see
[the Cloudflare token and multi-key guide](cloudflare-turn.md). Existing host nginx already owns 80/443. Add only the reviewed
server-name-specific routing for this domain; preserve other nginx hosts, ingress,
DNS, tunnels and services. Do not install a catch-all server. RemoteVNC uses `/v1`,
`/auth/google` and `/login/callback` at the origin root; prefix mounting is not
supported by current absolute enrollment URLs/forms.

Create a private deployment directory **outside git**. Copy compose.yaml and
`env.example` to `/etc/remotevnc-backend/{compose.yaml,deploy.env}` (root 0700
parent, deploy.env 0600), replace nonsecret IDs/absolute paths, then provision:

```sh
sudo install -d -m0700 /srv/remotevnc/secrets
sudo install -d -m0700 -o 10001 -g 10001 /srv/remotevnc/data
```

Create the two secret files named by deploy.env using a private secret manager or
editor. The Google file contains only the raw client secret (not its JSON download);
the TURN file contains the `keys` JSON described in the guide above. Their
owner must be UID 10001 with mode 0400/0600 and root 0700 containing directory, so
root Compose can bind-mount them while only the backend UID reads the mounted
file. Compose local file-secret mounts preserve host ownership; declaring secret
uid/mode is not a substitute for host permissions. The entrypoint requires both
Google client secret and either the TURN JSON file or legacy token configuration, rejects
conflicting sources, and never prints values. Never put live values in CLI args,
Docker build args, shell history, compose config output, CI variables in logs or
the repository. Rotate any previously exposed OAuth secret through the provider
before public deployment; live credential changes are a separate owner action.

Build the image from a clean, reviewed checkout, then start privately:

```sh
docker build -f packaging/backend/Dockerfile -t remotevnc-backend:local .
sudo docker compose --project-directory /etc/remotevnc-backend \
  --env-file /etc/remotevnc-backend/deploy.env \
  -f /etc/remotevnc-backend/compose.yaml up -d --no-build
```

Compose uses the separately built image; it has no build context or source-tree
mount. Keep its immutable image ID alongside deployment/change records. Required Google
configuration is validated before creating the DB or listening. Invalid/missing
HTTPS origin/auth fails closed; there is no installed auth bypass. Missing TURN
configuration is rejected by the packaged entrypoint (the library's unavailable
TURN mode remains useful for local tests only). Initial Google discovery needs
outbound HTTPS. Keep time synchronized; lease/token safety assumes local clock
and bounded scheduler operation. On restart existing sessions expire.

Image bases are pinned to multiarchitecture manifest digests, Rust 1.95.0 and a
Debian snapshot 2026-10-20 package snapshot; Cargo.lock + `--locked` pins Rust resolution.
`-j1` bounds build memory. Backend builds do not install native desktop/media
libraries. Snapshot/base/lockfile updates require review. This controls inputs;
byte-identical builds across different architectures/paths are not claimed.
[Docker secret documentation](https://docs.docker.com/compose/how-tos/use-secrets/)
describes the per-service file mounts used here.

## Additive ingress and privacy

After domain/path ownership review, merge `nginx-http.conf` into the existing
http block and `nginx-locations.conf` into the selected existing HTTPS server;
retain its existing certificate/TLS/other-site configuration. Review `nginx -t`
and only then arrange an authorized reload. These files are examples, never
installed by the Ubuntu installer. Validate callback redirect URI exactly with
Google; do not rewrite its query. Existing global access/error/WAF/CDN/tracing
logs must be reviewed as well: callback queries, Authorization, cookies and bodies
must never be logged at any layer. Location logs use `$uri`, never `$request` or
`$request_uri`; nginx location errors are suppressed because they can include the
original callback query. Avoid verbose upstream/native debugging with live data.

The example caps aggregate HTTP requests at 2/s with burst 30, body 16KiB and 64
concurrent requests/connections. The constant bucket is intentional behind
Cloudflare: it cannot be evaded by arbitrary client/X-Forwarded-For values. It
covers all identity endpoints, including public enrollment and challenge/poll,
and bounds ephemeral row creation; this conservative personal-deployment limit
may need review for more users. It is not distributed abuse prevention. With
hourly pruning and one-hour grace, ephemeral rows survive at most about two hours
past expiry while maintenance is healthy (roughly<=16k accepted requests over the
10-minute enrollment TTL plus two-hour retention window, plus bursts). Device-key
ownership tombstones are permanent and deliberately excluded from that bound.
Monitor maintenance failures, disk/free-list growth and request limits; turn off
public enrollment ingress if maintenance fails or its reserved disk quota nears
exhaustion. Rate limits do not replace authentication.

WebSocket Upgrade/Connection forwarding and 90s read timeout are explicit; see
[nginx WebSocket behavior](https://nginx.org/en/docs/http/websocket.html) and
[rate limiting](https://nginx.org/en/docs/http/ngx_http_limit_req_module.html).
Cloudflare/proxy idle EOF still requires bounded authenticated reconnect and
snapshot reconciliation; the implementation does not send an outbound Ping to
keep every proxy alive. Ping/activity never renews token/session authority.
Loopback tests do not establish long-lived proxy/real-TURN acceptance.

## Retention, backup and restore

Install `remotevnc-maintenance.{service,timer}` into `/etc/systemd/system`, then
review and explicitly enable its hourly timer. It invokes the private backend
container as its existing UID, uses BEGIN IMMEDIATE/busy timeout, and deletes
only expired OAuth transactions, confirmations, enrollments, challenges and
access tokens after one-hour grace; already closed/expired sessions are removed.
Live records, users, devices, public-key uniqueness and revoked ownership
**never** get pruned. The backend's per-process authorization gate is not bypassed:
maintenance never revokes/renews an active token or session. Do not run VACUUM
against active traffic; SQLite reuses freed pages. Alert on nonzero maintenance
exit and monitor allocated disk separately.

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now remotevnc-maintenance.timer
# Or run one maintenance pass explicitly using the same compose arguments above:
sudo docker compose --project-directory /etc/remotevnc-backend \
  --env-file /etc/remotevnc-backend/deploy.env \
  -f /etc/remotevnc-backend/compose.yaml exec -T backend \
  python3 /usr/local/bin/maintenance.py prune /data/backend.db
```

A consistent private backup uses SQLite's Online Backup API; copying a live DB
file alone can omit committed WAL. Choose a fresh destination; the tool refuses
overwrite/public/symlink sources or public backup parents and creates 0600 output:

```sh
sudo docker compose --project-directory /etc/remotevnc-backend \
  --env-file /etc/remotevnc-backend/deploy.env \
  -f /etc/remotevnc-backend/compose.yaml exec -T backend \
  python3 /usr/local/bin/maintenance.py backup /data/backend.db /data/backup-20261021.db
```

Backups contain private identities/tokens and permanent revocations. Export them
to encrypted private offline storage with a finite rotation policy (for example
seven daily and four weekly), and remove exported local backup files when safely
verified. Do not log their contents or serve `/data`. The tool performs
`integrity_check`; see [Python SQLite backup](https://docs.python.org/3/library/sqlite3.html#sqlite3.Connection.backup).

To restore, stop the backend, retain a fresh consistent backup, and replace its
DB with a verified 0600 UID 10001 backup in the private data directory. Remove
stale `-wal`/`-shm` sidecars only while all DB users/maintenance are stopped, then
restart one instance and check logs/auth/revocation. **Never restore an older
backup without reconciling device revocations made since that backup**: it would
resurrect revoked keys/ownership history. Restoring a deployment image alone must
not roll back its live data volume. Test restore offline before relying on a
backup. No automatic database rollback or tombstone deletion is provided.
