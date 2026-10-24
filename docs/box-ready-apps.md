# Making a web app box-ready

The box is a generic base for one web app per box: a Raspberry Pi Zero 2 W that
keeps itself and its app up to date and puts the app on the owner's Tailscale
network over HTTPS. The app is chosen once: on the box's setup page (by pasting
the app's GitHub link), or preset in a preinstalled image. From then on the box
only runs that app, and only releases signed with its publisher's key;
switching apps means reflashing the card.

## What the app has to be

- A single **static Linux binary for aarch64** (e.g. Rust with the
  `aarch64-unknown-linux-musl` target), in a `.tar.gz` attached to a GitHub
  release. Anything else in the tarball (e.g. `static/`) is unpacked next to it.
- It **listens on `BIND_ADDR`** (`127.0.0.1:3100`; `PORT` is `3100` too). The
  box forwards `https://<box>.<tailnet>.ts.net/` to it, with
  `X-Forwarded-Proto: https` and `X-Forwarded-For`, so Secure cookies work.
- It **keeps its data under `data/`** (relative to its working directory). The
  rest of the filesystem is read-only to it, it runs as an unprivileged user,
  and `data/` survives app and system updates. SQLite databases (`*.db`) in
  `data/` are snapshotted before each app update and restored if the new
  version doesn't come up.
- **`GET /` answers** (any 2xx/3xx) within 90 seconds of starting: that's how the
  box decides an update worked. Otherwise it puts the previous version and its
  data back.
- Optional: the [box API](box-api.md) on `$BOX_API_SOCKET` to show the box's
  update status inside the app or trigger updates and restarts.

## Publishing releases for boxes

Once, in the app's repo:

1. Make a signing key and keep it secret (a password manager, an offline backup;
   for CI, a repository secret). Boxes running the app only accept releases
   signed with it; lose it and they need reflashing to move to a new key.
   ```
   tools/app-release.sh new-key ~/my-app-box-key.pem
   ```
   Put the printed **fingerprint** in the app's README: owners compare it when
   they choose the app on their box.
2. Add a `box-app-spec.json` describing how to run the app, e.g.
   [kid-phone-server's](examples/kid-phone-server.box-app-spec.json):
   ```json
   {
     "name": "kid-phone-server",
     "asset": "kid-phone-server-aarch64-unknown-linux-musl.tar.gz",
     "command": "kid_phone_server",
     "env": { "DATABASE_URL": "sqlite://data/kidphone.db", "ADMIN_USERNAME": "admin" },
     "secrets": ["ADMIN_PASSWORD"],
     "first_login": { "username": "admin", "password": "ADMIN_PASSWORD" }
   }
   ```
   - `asset`: the release file with the binary; `command`: the binary inside it
     (default: the name with `-` as `_`).
   - `env`: written to the app's environment on first install. Later releases
     only add settings that are missing, so values the owner changed stay.
   - `secrets`: variables the box fills with a random 20-character value on
     first install. `first_login` names the one shown on the box's page as the
     app's first password (with `username`), until the owner hides it.

For each release (after its tarball is attached):
```
tools/app-release.sh sign --key ~/my-app-box-key.pem --spec box-app-spec.json \
    --repo OWNER/REPO --tag v1.2.3
```
This attaches `box-app.json` and `box-app.json.sig` to the release. To do it in
CI instead, copy [box-release.yml](examples/box-release.yml) into the app's
`.github/workflows/` and store the key as the secret `BOX_APP_KEY`. Boxes check
`releases/latest/download/box-app.json` a minute after starting and about every
12 hours, offer the update on their page and box API, and install it when the
owner says so (app updates are never automatic: they can change how things work).

**Test versions:** publish the release as a GitHub pre-release and add
`--prerelease`. That also updates the repo's rolling `box-testing` pre-release,
which only boxes set to "get test versions early" read; they take it when it's
newer than the latest regular release.

## Preinstalled images

To hand out a card image with the app already on it (it runs from the first
boot, no internet needed for the app):
```
tools/make-app-image.sh box-<version>.img.xz my-app-box.img.xz github.com/OWNER/REPO \
    --fingerprint "abcd ef01 2345 6789 abcd"
```
It takes the generic image (from this project's releases), the app's latest
signed release, and writes both the app choice and a copy of the release onto
the card's data partition. The result is otherwise the generic image: system
updates still come from this project, app updates from the app's releases.
Without `--fingerprint` the tool trusts whatever key the release names, so pass
it when building from a downloaded release.

## What the owner sees

Setup on the box's page: admin password, Wi-Fi, name, then **App**: paste the
app's link (`github.com/OWNER/REPO`), check the publisher fingerprint, install.
The App section then shows the running version, available updates, the first
sign-in password if the app asked for one, and where the app comes from. A
factory reset erases the app's data but keeps the choice of app.
