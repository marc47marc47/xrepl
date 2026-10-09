//! xrepl - exact, escape-free string replacement for AI agents.
//!
//! The old/new text is read from FILES (or stdin for one of them), never from
//! command-line arguments, so no shell / regex / JSON escaping is ever applied
//! to `\`, `/`, quotes or Windows paths.
//!
//! Safety rules:
//!   * Byte-exact matching (no regex, no normalization).
//!   * The number of matches must equal `--expect` (default 1); otherwise abort
//!     without touching the file.
//!   * Writes are atomic: temp file in the same directory, fsync, then rename.
//!   * Optional `--dry-run` prints match locations only.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Debug)]
enum XreplError {
    Usage(String),
    Io {
        path: PathBuf,
        source: io::Error,
    },
    EmptyPattern,
    MatchCount {
        expected: usize,
        found: usize,
        lines: Vec<usize>,
    },
    BothStdin,
}

impl fmt::Display for XreplError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            XreplError::Usage(msg) => write!(f, "usage error: {msg}\n\n{USAGE}"),
            XreplError::Io { path, source } => write!(f, "io error on {}: {source}", path.display()),
            XreplError::EmptyPattern => write!(f, "old text is empty; refusing to replace"),
            XreplError::MatchCount { expected, found, lines } => write!(
                f,
                "match count mismatch: expected {expected}, found {found} (lines: {lines:?}); file NOT modified"
            ),
            XreplError::BothStdin => write!(f, "only one of --old-file / --new-file may be '-' (stdin)"),
        }
    }
}

impl XreplError {
    fn exit_code(&self) -> u8 {
        match self {
            XreplError::Usage(_) | XreplError::BothStdin => 64,
            XreplError::Io { .. } => 74,
            XreplError::EmptyPattern => 65,
            XreplError::MatchCount { .. } => 2,
        }
    }
}

const USAGE: &str =
    "xrepl --target <file> --old-file <file|-> --new-file <file|-> [--expect N] [--dry-run]";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    Stdin,
    File(PathBuf),
}

#[derive(Debug)]
struct Config {
    target: PathBuf,
    old: Source,
    new: Source,
    expect: usize,
    dry_run: bool,
}

impl Config {
    fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Self, XreplError> {
        let mut target = None;
        let mut old = None;
        let mut new = None;
        let mut expect = 1usize;
        let mut dry_run = false;
        let mut it = args.into_iter();
        while let Some(arg) = it.next() {
            let mut value = |name: &str| {
                it.next()
                    .ok_or_else(|| XreplError::Usage(format!("{name} requires a value")))
            };
            match arg.as_str() {
                "--target" => target = Some(PathBuf::from(value("--target")?)),
                "--old-file" => old = Some(Self::source(value("--old-file")?)),
                "--new-file" => new = Some(Self::source(value("--new-file")?)),
                "--expect" => {
                    let v = value("--expect")?;
                    expect = v.parse().map_err(|_| {
                        XreplError::Usage(format!(
                            "--expect must be a non-negative integer, got {v}"
                        ))
                    })?;
                }
                "--dry-run" => dry_run = true,
                "-h" | "--help" => return Err(XreplError::Usage("help requested".into())),
                other => return Err(XreplError::Usage(format!("unknown argument: {other}"))),
            }
        }
        let cfg = Config {
            target: target.ok_or_else(|| XreplError::Usage("--target is required".into()))?,
            old: old.ok_or_else(|| XreplError::Usage("--old-file is required".into()))?,
            new: new.ok_or_else(|| XreplError::Usage("--new-file is required".into()))?,
            expect,
            dry_run,
        };
        if cfg.old == Source::Stdin && cfg.new == Source::Stdin {
            return Err(XreplError::BothStdin);
        }
        Ok(cfg)
    }

    fn source(v: String) -> Source {
        if v == "-" {
            Source::Stdin
        } else {
            Source::File(PathBuf::from(v))
        }
    }
}

fn read_source(src: &Source) -> Result<Vec<u8>, XreplError> {
    match src {
        Source::Stdin => {
            let mut buf = Vec::new();
            io::stdin()
                .read_to_end(&mut buf)
                .map_err(|e| XreplError::Io {
                    path: PathBuf::from("<stdin>"),
                    source: e,
                })?;
            Ok(buf)
        }
        Source::File(p) => fs::read(p).map_err(|e| XreplError::Io {
            path: p.clone(),
            source: e,
        }),
    }
}

/// Byte-exact, non-overlapping match finder.
struct Matcher<'a> {
    needle: &'a [u8],
}

impl<'a> Matcher<'a> {
    fn new(needle: &'a [u8]) -> Result<Self, XreplError> {
        if needle.is_empty() {
            return Err(XreplError::EmptyPattern);
        }
        Ok(Self { needle })
    }

    fn find_all(&self, hay: &[u8]) -> Vec<usize> {
        let n = self.needle.len();
        let mut out = Vec::new();
        let mut i = 0;
        while i + n <= hay.len() {
            if &hay[i..i + n] == self.needle {
                out.push(i);
                i += n;
            } else {
                i += 1;
            }
        }
        out
    }
}

fn line_of(hay: &[u8], offset: usize) -> usize {
    hay[..offset].iter().filter(|&&b| b == b'\n').count() + 1
}

fn replace_at(hay: &[u8], positions: &[usize], old_len: usize, new: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(hay.len() + positions.len() * new.len());
    let mut last = 0;
    for &p in positions {
        out.extend_from_slice(&hay[last..p]);
        out.extend_from_slice(new);
        last = p + old_len;
    }
    out.extend_from_slice(&hay[last..]);
    out
}

fn atomic_write(path: &Path, data: &[u8]) -> Result<(), XreplError> {
    let io_err = |e| XreplError::Io {
        path: path.to_path_buf(),
        source: e,
    };
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| XreplError::Usage("target has no file name".into()))?;
    let tmp = dir.join(format!(
        ".{}.xrepl.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    let perms = fs::metadata(path).map_err(io_err)?.permissions();
    {
        let mut f = File::create(&tmp).map_err(|e| XreplError::Io {
            path: tmp.clone(),
            source: e,
        })?;
        f.write_all(data).map_err(|e| XreplError::Io {
            path: tmp.clone(),
            source: e,
        })?;
        f.sync_all().map_err(|e| XreplError::Io {
            path: tmp.clone(),
            source: e,
        })?;
    }
    fs::set_permissions(&tmp, perms).map_err(|e| XreplError::Io {
        path: tmp.clone(),
        source: e,
    })?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(io_err(e));
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    DryRun { lines: Vec<usize> },
    Replaced { count: usize, lines: Vec<usize> },
}

fn run(cfg: &Config) -> Result<Outcome, XreplError> {
    let old = read_source(&cfg.old)?;
    let new = read_source(&cfg.new)?;
    let hay = fs::read(&cfg.target).map_err(|e| XreplError::Io {
        path: cfg.target.clone(),
        source: e,
    })?;
    let matcher = Matcher::new(&old)?;
    let positions = matcher.find_all(&hay);
    let lines: Vec<usize> = positions.iter().map(|&p| line_of(&hay, p)).collect();
    if positions.len() != cfg.expect {
        return Err(XreplError::MatchCount {
            expected: cfg.expect,
            found: positions.len(),
            lines,
        });
    }
    if cfg.dry_run {
        return Ok(Outcome::DryRun { lines });
    }
    let out = replace_at(&hay, &positions, old.len(), &new);
    atomic_write(&cfg.target, &out)?;
    Ok(Outcome::Replaced {
        count: positions.len(),
        lines,
    })
}

fn main() -> ExitCode {
    let result = Config::parse(std::env::args().skip(1)).and_then(|cfg| run(&cfg));
    match result {
        Ok(Outcome::DryRun { lines }) => {
            println!("dry-run: {} match(es) at lines {:?}", lines.len(), lines);
            ExitCode::SUCCESS
        }
        Ok(Outcome::Replaced { count, lines }) => {
            println!("replaced {count} match(es) at lines {lines:?}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("xrepl: {e}");
            ExitCode::from(e.exit_code())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xrepl-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn setup(tag: &str, target: &[u8], old: &[u8], new: &[u8]) -> (PathBuf, Config) {
        let d = tmpdir(tag);
        fs::write(d.join("t.txt"), target).unwrap();
        fs::write(d.join("old"), old).unwrap();
        fs::write(d.join("new"), new).unwrap();
        let cfg = Config {
            target: d.join("t.txt"),
            old: Source::File(d.join("old")),
            new: Source::File(d.join("new")),
            expect: 1,
            dry_run: false,
        };
        (d, cfg)
    }

    #[test]
    fn replaces_windows_path_with_backslashes_verbatim() {
        let (d, cfg) = setup(
            "winpath",
            b"let p = \"C:\\\\Users\\\\marc\\\\a.txt\";\n",
            b"C:\\\\Users\\\\marc\\\\a.txt",
            b"D:/data/b.txt",
        );
        assert_eq!(
            run(&cfg).unwrap(),
            Outcome::Replaced {
                count: 1,
                lines: vec![1]
            }
        );
        assert_eq!(
            fs::read(d.join("t.txt")).unwrap(),
            b"let p = \"D:/data/b.txt\";\n"
        );
    }

    #[test]
    fn regex_metachars_are_literal() {
        let (d, cfg) = setup("meta", b"a.*b\nx(y)|z\n", b"x(y)|z", b"$1\\n&");
        run(&cfg).unwrap();
        assert_eq!(fs::read(d.join("t.txt")).unwrap(), b"a.*b\n$1\\n&\n");
    }

    #[test]
    fn refuses_on_multiple_matches_and_leaves_file_untouched() {
        let (d, cfg) = setup("multi", b"foo\nfoo\n", b"foo", b"bar");
        match run(&cfg) {
            Err(XreplError::MatchCount {
                expected: 1,
                found: 2,
                lines,
            }) => assert_eq!(lines, vec![1, 2]),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(fs::read(d.join("t.txt")).unwrap(), b"foo\nfoo\n");
    }

    #[test]
    fn expect_n_replaces_all() {
        let (d, mut cfg) = setup("expectn", b"foo\nfoo\n", b"foo", b"bar");
        cfg.expect = 2;
        run(&cfg).unwrap();
        assert_eq!(fs::read(d.join("t.txt")).unwrap(), b"bar\nbar\n");
    }

    #[test]
    fn zero_matches_is_error() {
        let (_d, cfg) = setup("zero", b"abc", b"zzz", b"y");
        assert!(matches!(
            run(&cfg),
            Err(XreplError::MatchCount { found: 0, .. })
        ));
    }

    #[test]
    fn crlf_is_matched_byte_exact() {
        let (d, cfg) = setup("crlf", b"a\r\nb\r\n", b"a\r\nb", b"c\r\nd");
        run(&cfg).unwrap();
        assert_eq!(fs::read(d.join("t.txt")).unwrap(), b"c\r\nd\r\n");
    }

    #[test]
    fn lf_pattern_does_not_match_crlf_file() {
        let (_d, cfg) = setup("crlf2", b"a\r\nb\r\n", b"a\nb", b"x");
        assert!(matches!(
            run(&cfg),
            Err(XreplError::MatchCount { found: 0, .. })
        ));
    }

    #[test]
    fn dry_run_does_not_modify() {
        let (d, mut cfg) = setup("dry", b"x/y\\z\n", b"/y\\", b"Q");
        cfg.dry_run = true;
        assert_eq!(run(&cfg).unwrap(), Outcome::DryRun { lines: vec![1] });
        assert_eq!(fs::read(d.join("t.txt")).unwrap(), b"x/y\\z\n");
    }

    #[test]
    fn empty_old_rejected() {
        let (_d, cfg) = setup("empty", b"abc", b"", b"y");
        assert!(matches!(run(&cfg), Err(XreplError::EmptyPattern)));
    }

    #[test]
    fn non_overlapping_count() {
        let m = Matcher::new(b"aa").unwrap();
        assert_eq!(m.find_all(b"aaaa"), vec![0, 2]);
    }

    #[test]
    fn parse_rejects_double_stdin() {
        let args = ["--target", "t", "--old-file", "-", "--new-file", "-"].map(String::from);
        assert!(matches!(Config::parse(args), Err(XreplError::BothStdin)));
    }
}
