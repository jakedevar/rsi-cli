`rsi-manual.md` and `rsi-manual.html` are generated from the rsi registries; never edit them by hand.
Regenerate them (and the `rsi:generated` regions of `docs/keybindings.md`) with `make manual`; drift tests fail when they are stale.
Print: open `rsi-manual.html` in a browser and print to PDF, or run `make manual-pdf` (needs `pandoc`).
