# The box API (for kid-phone-server)

On the box, kid-phone-server doesn't manage the OS itself: there's no `apt`, no
root watcher script, and the system files are read-only. Instead the box's agent
does updates (signed, A/B, with rollback), and the app asks it what's available
and to install things through this API.

## Is the app running on a box?

The box starts the app with `BOX_API_SOCKET=/run/box/api.sock`. If that variable
is set, use this API; if not, keep the current behaviour (apt, flag files) for
installs on a regular Pi.

## What else is different on the box

- **Address:** keep `BIND_ADDR=127.0.0.1:3100`. People reach the app at
  `https://<box>.<tailnet>.ts.net/` through `tailscale serve`; the box's agent
  forwards to the app with `X-Forwarded-Proto: https`, so Secure cookies work.
  Don't set `INSECURE_COOKIES`.
- **Files:** the working directory is `/data/app/run`, where `data/` is the
  app's own persistent folder (`/data/app/state`) and `static/` comes from the
  installed release. Everything the app writes must go under `data/`; the rest
  of the filesystem is read-only to it.
- **First run:** the box generates `ADMIN_PASSWORD` and shows it on the box's
  system page (`http://box.local/_system/`) until the owner hides it.
- **Not available on the box:** the flag-file watcher (`data/update_requested`)
  and its actions, `apt`, `/var/run/reboot-required`, `tailscale update`, and
  formatting external drives. Use the API below for updates and restarts.
- **Health:** after an app update the box waits up to 90 s for `GET /` on
  `BIND_ADDR` to answer (redirects followed; any error status means "broken")
  and otherwise restores the database snapshot and switches back. A cheap
  `GET /healthz` returning 200 would make this more precise; tell the box about
  it and it'll use that instead.

## Endpoints

HTTP/1.1 over the Unix socket. No login: only root and the app's user can open
the socket. Bodies are JSON.

### `GET /v1/status`

```json
{
  "os": {
    "version": "2026.10.19.2354-gad264b7",
    "latest": "2026.10.24.0300-g1234abc",
    "update_available": true,
    "install": { "state": "idle" }
  },
  "app": {
    "version": "0.18.6",
    "latest": "0.19.0",
    "update_available": true,
    "progress": { "state": "idle" }
  },
  "tailscale": {
    "version": "1.104.1",
    "backend_state": "Running",
    "dns_name": "box.tail1234.ts.net",
    "progress": { "state": "idle" }
  },
  "checked_at": 1792468800,
  "check_error": null,
  "checking": false,
  "restart_needed": false,
  "auto_updates": { "enabled": true, "timezone": "America/New_York" },
  "last_night": { "date": "2026-10-24", "summary": "Installed system 2026.10.24.0300-g1234abc. Restarted to finish." }
}
```

- `os.install.state`: `idle`, `running` (with `percent`, `step`), `installed`
  (waiting for a restart; `restart_needed` is true), or `failed` (with `error`;
  nothing was changed).
- `app.progress.state` and `tailscale.progress.state`: `idle`, `checking`,
  `downloading` (`version`, `percent`), `installing` (`version`), `updated`
  (`version`), `up_to_date` (`version`), or `failed` (`error`).
- `latest` values come from the box's signed release manifest, checked a minute
  after boot and about every 12 hours. `checked_at` is a Unix time; `null`
  until the first successful check.
- `auto_updates`: whether the box installs system and Tailscale updates by
  itself between 2 and 5 am in `timezone` (`null` until the owner picks one on
  the box's page; then UTC), restarting after a system update. App updates are
  never automatic. `last_night` says what the last run did (`null` before the
  first).

### Actions

All are `POST` with an empty body. `202` means started (watch `/v1/status`),
`409` means one is already running, `404` means there's no release to install
yet (check first).

| Endpoint | Does |
|---|---|
| `POST /v1/check` | Check for updates now |
| `POST /v1/os/install` | Install the OS update from the manifest (into the other slot; takes effect after a restart) |
| `POST /v1/app/update` | Install the app update from the manifest. The app is restarted onto the new version; if it doesn't come up, the box restores the database and the previous version. |
| `POST /v1/tailscale/update` | Update Tailscale to its latest signed release (rolled back if it doesn't start) |
| `POST /v1/restart` | Restart the box (answers first, restarts a second later) |

Note that `POST /v1/app/update` restarts the app itself: the request returns
`202` before that happens, and the app should expect to be stopped shortly after.

## Calling it from Rust

No extra crates needed; a one-shot HTTP/1.0 request over tokio's `UnixStream`:

```rust
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Calls the box API; returns (status code, body).
async fn box_api(method: &str, path: &str) -> std::io::Result<(u16, String)> {
    let socket = std::env::var("BOX_API_SOCKET").map_err(std::io::Error::other)?;
    let mut stream = tokio::net::UnixStream::connect(socket).await?;
    let request = format!("{method} {path} HTTP/1.0\r\nHost: box\r\nContent-Length: 0\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;
    let mut response = String::new();
    stream.read_to_string(&mut response).await?; // HTTP/1.0: the box closes when done
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
    let code = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    Ok((code, body.to_owned()))
}

// let (code, body) = box_api("GET", "/v1/status").await?;
// let status: serde_json::Value = serde_json::from_str(&body)?;
// box_api("POST", "/v1/app/update").await?;
```

## Mapping the current "Software updates" page

| Today (regular Pi) | On the box |
|---|---|
| App version vs GitHub latest | `app.version` / `app.latest` / `app.update_available` |
| "Update" button → `update` flag | `POST /v1/app/update` |
| `apt list --upgradable` count | `os.update_available` (one signed OS image, not packages) |
| "Upgrade OS" → `os_upgrade` flag | `POST /v1/os/install`, then `POST /v1/restart` when `restart_needed` |
| `/var/run/reboot-required` | `restart_needed` |
| `tailscale version --upstream` | `tailscale.version`; updates via `POST /v1/tailscale/update` |
| "Reboot" → `reboot` flag | `POST /v1/restart` |
| Watcher version / schema warnings | Not applicable: hide them when `BOX_API_SOCKET` is set |
| Update schedule (`schedule.conf`) | `auto_updates`: the box installs system and Tailscale updates at night by itself (set on the box's page) |
