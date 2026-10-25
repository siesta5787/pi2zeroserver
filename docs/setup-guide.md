# Setting up a Box

A Box is a Raspberry Pi Zero 2 W that runs one web app, for example Kids
Device MDM, and keeps itself up to date. You set it up once from your phone,
and you never need a keyboard, a screen or a terminal. This takes about 20
minutes.

## What you need

- A **Raspberry Pi Zero 2 W** and a **USB power supply** for it (micro-USB, 2.5 A).
- A **microSD card**, 8 GB or larger.
- A computer with an SD card slot (or a USB card reader), to copy the Box onto the card.
- Your **phone**.
- Your home **Wi-Fi name and password**. The Pi only uses 2.4 GHz Wi-Fi: most
  routers have it, but a 5 GHz-only network won't show up.
- A free **Tailscale** account. You'll make it in step 4 if you don't have one.

## 1. Put the Box on the card

1. Download **box-…img.xz** from the latest release:
   <https://github.com/siesta5787/pi2zeroserver/releases/latest>.
   Some apps offer their own Box image with the app already on it. That works
   the same way, and you can skip step 5.
2. Install **Raspberry Pi Imager** (<https://www.raspberrypi.com/software/>) and open it.
3. For **Device**, choose *Raspberry Pi Zero 2 W*. For **Operating system**,
   scroll down to *Use custom* and pick the file you downloaded. For
   **Storage**, choose your SD card.
4. If Imager offers to customise settings (Wi-Fi, user name and so on),
   choose **No**: the Box sets itself up from your phone instead. Then confirm
   and wait for Imager to write and verify the card.

## 2. Start it

Put the card in the Pi. Plug the power supply into the micro-USB port marked
**PWR IN**, the one nearest the corner. The first start takes 2–3 minutes.

## 3. Connect it to your Wi-Fi

1. On your phone, open the Wi-Fi settings and join **Box-Setup-XXXX** (XXXX
   is different for each box). Its password is **boxsetup**.
2. The Box's page usually opens by itself. If it doesn't, open
   **http://192.168.4.1/** in your phone's browser. Your phone may say this
   network has no internet: that's expected, so stay connected.
3. **Choose a password** for the Box. You'll need it to change settings, so
   save it in your password manager.
4. Tap **Choose your Wi-Fi network**, pick your home network, type its
   password and tap **Join**.
5. The Box leaves its setup network, so your phone loses the page. Reconnect
   your phone to your home Wi-Fi, then open **http://box.local/_system/** and
   sign in.

If you typed the Wi-Fi password wrong, *Box-Setup-XXXX* comes back within a
couple of minutes: join it again and the page says what went wrong. If
`box.local` doesn't open (some Android phones can't use these names), find
"box" in your router's list of devices and open its address instead.

**Optional: give the Box a name.** Under **Name**, tap **Rename**. That name is
used everywhere from then on, including Tailscale, so do it before step 4.
It's handy if you'll have more than one box.

## 4. Set up Tailscale (remote access)

The app isn't open to your home network or the internet. You reach it through
Tailscale, a free service that links your own devices privately, at home or
away.

1. **Make an account** at <https://tailscale.com>. You can sign up with
   Google, Microsoft, Apple or GitHub.
2. **Turn on HTTPS for your account:** in the Tailscale admin console, open
   **DNS** (<https://login.tailscale.com/admin/dns>). Check that **MagicDNS** is
   on, and under **HTTPS Certificates** click **Enable HTTPS**. You only do this
   once per account.
3. **Connect the Box:** on the Box's page, under **Remote access (Tailscale)**,
   tap **Connect Tailscale**. Then tap **Sign in to Tailscale**, or scan the QR
   code with another device. Sign in with the same account and confirm adding
   the device. After a few seconds the Box's page shows **Connected**.
4. **Keep it connected:** Tailscale signs devices out after 180 days unless you
   switch that off. In the admin console, open **Machines**, then the **…**
   menu next to your box, and choose **Disable key expiry**.
5. **Add your own devices:** install the Tailscale app on your phone and on any
   computer you'll use the app from, sign in with the same account, and switch
   it on.

The Box's page now shows **its settings address**, e.g.
`https://box.your-tailnet.ts.net/_system/`. Bookmark it: it works from
anywhere, on any of your devices with Tailscale on. While Tailscale is on, your
phone can't open `box.local`: use the address above, or simply `http://box/`.

## 5. Choose the app

Skip this if your image came with the app already on it.

1. On the Box's page, under **App**, paste the app's link and tap **Look it
   up**. For Kids Device MDM, that's
   `github.com/siesta5787/kid-phone-server`.
2. The Box shows the app and its publisher's **key fingerprint**. Check that it
   matches the one in the app's instructions (for Kids Device MDM:
   `344a e11a 0a68 15f2 5b14`), then tap **Install**.

   This is a one-time choice. From then on the Box only runs this app, and only
   versions its publisher signed. Running a different app means flashing the
   card again.
3. The app installs in a minute or two. The **App** section then shows **Open
   the app at https://box.your-tailnet.ts.net/** and, for apps that have one, a
   first username and password.
4. Open the app, sign in with those, and follow its own first steps (change the
   password, and so on). Back on the Box's page, tap **I've signed in, hide this**.

You're done.

## Day to day

- **System and Tailscale updates** install by themselves at night (2–5 am).
  Under **Updates → Automatic updates → Change**, set your time zone so "night"
  is your night, or switch them off.
- **App updates** never install by themselves, because a new version can change
  how the app works. When one is out, the Box's page (and, in Kids Device MDM,
  Settings → Software updates) offers **Update the app**. If the new version
  doesn't start, the Box puts the previous one and its data back.
- Leave the Box plugged in. It copes with power cuts, but avoid pulling the
  plug in the middle of an update.

## When something's wrong

- **New router or Wi-Fi password:** when the Box can't reach its Wi-Fi for 3
  minutes, it opens *Box-Setup-XXXX* again. Join it and choose the new network,
  as in step 3.
- **Forgot the Box's password:** unplug the Box and put its card in a
  computer. On the card's small **BOOT** drive, create an empty file named
  `reset-password.txt`. Put the card back and start the Box: its page asks for
  a new password, and everything else is kept.
- **"Opening the app over Tailscale: not working yet":** HTTPS isn't on for
  your Tailscale account. See step 4.2. The Box retries every minute.
- **Start over:** on the Box's page, *Erase everything* (at the bottom), or a
  `factory-reset.txt` file on the BOOT drive. Either one erases Wi-Fi,
  Tailscale, the app's data and the password; the box then opens
  *Box-Setup-XXXX* like a new one. The app choice stays. Also delete the old
  box from the Machines page in the Tailscale admin console.
- **Still stuck:** plug a screen into the Pi's mini-HDMI port. It shows the
  Box's messages, such as whether its setup network started.
