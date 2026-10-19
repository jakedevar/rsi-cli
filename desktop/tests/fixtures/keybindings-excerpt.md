# rsi Keybindings Reference

## Global Keybindings

<!-- rsi:generated:begin raw -->
**Clipboard paste**

| Chord | Context | Effect |
| --- | --- | --- |
| `Ctrl-V` | any context | paste the clipboard |
| `Enter` | quick-launch input | refused with a notice |
| `Enter` | create form | refused with a notice |
<!-- rsi:generated:end -->

## Normal Mode Keybindings

### All Normal-Mode Keys (generated)

<!-- rsi:generated:begin normal -->
| Keys | Action | What it does |
| --- | --- | --- |
| `?` | Contextual help | Opens help. |
| `<Space>gg` | Git panel | Runs lazygit. |
| `<Space>hs` / `:pause soft` | not a registry row shape | still one key row |
<!-- rsi:generated:end -->

### Session Management

| Key | Action | Description |
|-----|--------|-------------|
| `x` | SoftInterruptSession | hand-written table: no key row, only the section row |

### Leader Key and G-Chord Migration (`<Space>`)

| Before | After | Action / disposition |
|--------|-------|---------------------|
| `<Space>gg` | `<Space>gg` | ToggleGitPanel (already in the generated table) |
| `<Space>zz` | `<Space>zz` | Only here: one leader row |
| `gX` | `gX` | retired: not a leader row |

#### Nested heading

```
| Chord | Context | Effect |
| `Ghost` | inside a fence | never parsed |
```

## Command Mode

### rsi-Specific Commands

| Command | Aliases | Action | What it does |
| --- | --- | --- | --- |
| `:help` | `:keys` | Contextual help | Opens help. |
| `:manual [arg]` | `:man [arg]` | Open manual | Opens the manual. |
| `:manager board` |  | Open board | Opens the board. |
| `:manager inbox <id>` |  | Open inbox | Opens the inbox. |

## Overlay Keybindings

### Question Panel

| Key | Action |
| --- | --- |
| `Enter` | answer |
| `Esc` | decline |

#### Insert Mode (an overlay h4 is not a row)

| Key | Action |
| --- | --- |
| `i` | insert |

### Sort Picker Overlay (`<Space>s`)

| Key | Action |
| --- | --- |
| `j` | down |

## Overlay Key Reference (generated)

### Sort Picker

| Keys | Action | What it does |
| --- | --- | --- |
| `j` | Down | Moves down. |
| `k` | Up | Moves up. |
