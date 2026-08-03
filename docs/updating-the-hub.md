# Updating the Floptle Hub

**From v0.21.2 the Hub updates itself** — a banner offers it, one button
downloads and restarts, done. This page is for the one manual update that gets
you *to* v0.21.2, because a Hub older than that has no code to update itself
with and cannot see that an update exists.

Do this once. After it, you never do it again.

---

## Nothing you have is at risk

The Hub is a **single file**. Everything it remembers lives somewhere else, so
replacing it keeps your projects list, your installed engines, your settings and
your Foverse sign-in:

| | Linux | macOS | Windows |
|---|---|---|---|
| settings + projects | `~/.config/floptle/hub.json` | `~/Library/Application Support/com.Fopull.Floptle/hub.json` | `%APPDATA%\Fopull\Floptle\config\hub.json` |
| installed engines | `~/.local/share/floptle/versions/` | `~/Library/Application Support/com.Fopull.Floptle/versions/` | `%APPDATA%\Fopull\Floptle\data\versions\` |
| your sign-in | OS keyring | Keychain | Credential Manager |

None of those are touched. You are replacing one executable.

## Which version am I on?

Open the Hub → **About**. It says `version 0.21.1` or similar. If it says
0.21.2 or newer you already have self-updating and can stop reading — use the
banner.

## Get the download

All three platforms start here:

**<https://github.com/Fopull-LLC/Floptle-releases/releases/latest>**

Under **Assets**, take the one that matches your machine:

| your machine | file |
|---|---|
| Linux | `floptle-hub-<version>-linux-x86_64.tar.gz` |
| Mac, Apple Silicon (M1/M2/M3/M4) | `floptle-hub-<version>-macos-aarch64.tar.gz` |
| Mac, Intel | `floptle-hub-<version>-macos-x86_64.tar.gz` |
| Windows | `floptle-hub-<version>-windows-x86_64.zip` |

Not sure which Mac you have? **Apple menu → About This Mac**. "Apple M…" means
Apple Silicon; "Intel" means Intel. (Or run `uname -m` — `arm64` vs `x86_64`.)

The `floptle-…` archives *without* `hub` in the name are the engine. You don't
need those — the Hub installs engines for you.

---

## Linux

Quit the Hub first, then, with `VERSION` set to the one you downloaded:

```sh
cd ~/Downloads
tar -xzf floptle-hub-VERSION-linux-x86_64.tar.gz
chmod +x floptle-hub
```

That leaves a `floptle-hub` binary in the folder. **Put it wherever your old one
was** — if you don't know, search for it:

```sh
find ~ -name floptle-hub -type f 2>/dev/null
```

and overwrite that path with the new file. Then run it.

If you'd like it on your PATH so `floptle-hub` works from any terminal:

```sh
mkdir -p ~/.local/bin && mv floptle-hub ~/.local/bin/
```

## macOS

Quit the Hub first. Double-click the `.tar.gz` to unpack it, or:

```sh
cd ~/Downloads
tar -xzf floptle-hub-VERSION-macos-aarch64.tar.gz   # or -macos-x86_64
chmod +x floptle-hub
```

**Then remove the quarantine flag**, or macOS will refuse to open it:

```sh
xattr -d com.apple.quarantine floptle-hub
```

> **Why:** the Hub isn't signed with an Apple Developer certificate yet, so
> Gatekeeper blocks anything downloaded from the web. You'll otherwise see
> *"floptle-hub cannot be opened because it is from an unidentified developer"*
> or *"Apple could not verify…"*. The command above marks the file as one you
> chose to trust.
>
> The point-and-click equivalent: **right-click** (or Control-click) the file →
> **Open** → **Open** in the dialog. Double-clicking will not offer that choice;
> right-clicking is what makes the "Open anyway" button appear.

Move it over your old copy and run it.

## Windows

Quit the Hub first. Right-click the `.zip` → **Extract All**. Inside is
`floptle-hub.exe`. Copy it over your old `floptle-hub.exe`, replacing it.

> **If Windows blocks it:** you'll get a blue *"Windows protected your PC"*
> box. Click **More info** → **Run anyway**. This is SmartScreen reacting to an
> executable it hasn't seen many people run yet, not a virus warning.
>
> If it won't overwrite and says the file is in use, the Hub is still running —
> close it (check the system tray) and try again.

---

## After this

Open the Hub → **About**. It should say the new version, and *"This is the
newest Hub."*

From here on, when a new Hub ships you get:

- a **banner** when you open the Hub, with **Update and restart**
- a **green chip in the tab bar** that stays until you've updated — it doesn't
  go away when you dismiss the banner, on purpose
- **About**, which always answers whether you're current

One click, and it reopens itself.

## If "Update and restart" won't work later

The Hub tells you why rather than failing halfway. The usual cause is that it
lives somewhere your user can't write — `/usr/local/bin`, `/Applications`,
`C:\Program Files`. Either move the Hub somewhere you own (your home folder is
fine) or repeat the manual steps above. The button is hidden in that case rather
than shown and broken.
