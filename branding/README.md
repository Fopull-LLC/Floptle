# Branding

`floptle-logo.png` is the logo — the mark above the wordmark, white line art
with its shadow, on transparency. Everything else here is derived from it by
`scripts/brand/make-icons.py` and committed, so a binary embeds bytes that are
checked in rather than the output of a build step:

| file | what it is for |
| --- | --- |
| `icon-<size>.png` | the app icon: the mark on a dark rounded tile, 16 to 1024 px — the window icon (editor, player, Hub) and the Linux `hicolor` set the Hub installs |
| `floptle.ico` | the Windows executable's own icon, compiled in by each binary's `build.rs` |
| `floptle.icns` | for a macOS `.app` bundle, when the release builds one |

`crates/floptle-brand` embeds them and knows the three places an icon has to
be put — the window, the executable, and the desktop's own entry — and why a
Wayland desktop only honours the last of those. Change the logo, run the
script, commit the results.
