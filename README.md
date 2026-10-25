# Box

A small operating system for the Raspberry Pi Zero 2 W that runs one web app,
such as [Kids Device MDM](https://github.com/siesta5787/kid-phone-server), and
keeps it private and up to date by itself:

- set up from your phone, with no keyboard, screen or terminal
- the app is only reachable through your own [Tailscale](https://tailscale.com)
  network, never from the internet or your home network
- signed system, Tailscale and app updates, with automatic rollback if one
  doesn't start; system updates install at night

**Setting one up:** [docs/setup-guide.md](docs/setup-guide.md). Download the
card image from the [latest release](https://github.com/siesta5787/pi2zeroserver/releases/latest).

**Making an app run on a Box:** [docs/box-ready-apps.md](docs/box-ready-apps.md)
and [docs/box-api.md](docs/box-api.md).

**Building the Box itself:** [CLAUDE.md](CLAUDE.md) (build, test and release)
and [PLAN.md](PLAN.md) (design).
