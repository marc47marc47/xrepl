# xrepl

Exact, escape-free string replacement for AI agents and scripts.

The old/new text is read from **files** (or stdin for one of them), never from
command-line arguments, so no shell, regex or JSON escaping is ever applied to
`\`, `/`, quotes or Windows paths.

## Usage

```
xrepl --target <file> --old-file <file|-> --new-file <file|-> [--expect N] [--dry-run]
```

| Option | Description |
| --- | --- |
| `--target <file>` | File to modify. |
| `--old-file <file\|->` | File containing the exact text to find (`-` = stdin). |
| `--new-file <file\|->` | File containing the replacement text (`-` = stdin). |
| `--expect N` | Required number of matches (default `1`). |
| `--dry-run` | Only print match locations; do not modify the file. |
| `-h`, `--help` | Show usage. |

Only one of `--old-file` / `--new-file` may be `-`.

### Example

```sh
printf '%s' 'C:\old\path' > old.txt
printf '%s' 'C:\new\path' > new.txt
xrepl --target config.ini --old-file old.txt --new-file new.txt
```

## Safety guarantees

- Byte-exact matching: no regex, no normalization (CRLF/LF preserved).
- The match count must equal `--expect`, otherwise the file is **not** touched.
- Empty old text is refused.
- Atomic writes: temp file in the same directory, fsync, then rename.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Success |
| 2 | Match count mismatch |
| 64 | Usage error |
| 65 | Empty old text |
| 74 | I/O error |

## Install

Download a prebuilt binary (Linux, macOS, Windows; x86_64, aarch64 and more)
from the [Releases](https://github.com/marc47marc47/xrepl/releases) page, or build from source:

```sh
cargo install --git https://github.com/marc47marc47/xrepl
```
