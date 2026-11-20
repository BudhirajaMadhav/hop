# Hop

A tiny link router for macOS 26, written in Rust. It is a native AppKit app with no webview, and the app bundle is under 500 KB.

Make Hop your default browser. Each link then follows a rule, or it opens a Liquid Glass picker under the cursor.

```sh
./bundle.sh            # builds target/Hop.app
./bundle.sh install    # also replaces /Applications/Hop.app and opens it; the first time, macOS asks to make it the default browser
```

**Picker:** `1`–`9` (or `⌘1`–`⌘9`) or a click opens a top choice · `0` opens More, a menu of the rest · `↩` opens the selected row · `space` or a click ticks Remember for this domain (holding `⌥` does the same) · `⌘C` copies the link · `esc` closes · `⌘,` opens the rules file · `⌘Q` quits.

**Rules** are in `~/.config/hop/rules`. Open Hop.app again to edit the file.

```
+Google Chrome (Work)                      # pin a top choice; the rest go under More
+Safari
github.com/acme = Google Chrome (Work)   # domain[/path] = picker label | app name
youtube.com = Safari
figma.com = Figma
!T3 Code (Beta)                            # hide a row
```

Hop also removes tracking parameters (`utm_*`, `fbclid`, `gclid`, …) before it opens a link. `hop --list` prints the picker labels.
