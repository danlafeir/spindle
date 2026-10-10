use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

/// The key that starts every spindle command, as the single control byte the terminal sends.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prefix(u8);

impl Default for Prefix {
    fn default() -> Self {
        Prefix(0x02) // Ctrl-b
    }
}

impl Prefix {
    pub fn byte(self) -> u8 {
        self.0
    }

    /// The short form shown in the sidebar menu, such as `^b`.
    pub fn caret(self) -> String {
        format!("^{}", self.letter())
    }

    /// The long form shown in help, such as `Ctrl-b`.
    pub fn name(self) -> String {
        format!("Ctrl-{}", self.letter())
    }

    fn letter(self) -> char {
        char::from(self.0 + b'a' - 1)
    }

    /// Parses `C-a` through `C-z`, either case.
    fn parse(s: &str) -> Result<Prefix> {
        let letter = match s.strip_prefix("C-").map(str::as_bytes) {
            Some(&[c]) if c.is_ascii_alphabetic() => c.to_ascii_lowercase(),
            _ => bail!("prefix {s:?} isn't a control key; use the form \"C-a\""),
        };
        // These share a byte with a key the shell needs: Backspace on some terminals, Tab
        // and Enter. Each would then take two presses to reach the shell.
        if matches!(letter, b'h' | b'i' | b'j' | b'm') {
            bail!("prefix {s:?} is the same byte as Backspace, Tab or Enter");
        }
        Ok(Prefix(letter - b'a' + 1))
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct Config {
    pub prefix: Prefix,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    prefix: Option<String>,
}

/// `$XDG_CONFIG_HOME/spindle/config.toml`, else `~/.config/spindle/config.toml`, on macOS too.
pub fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".config")))?;
    Some(base.join("spindle").join("config.toml"))
}

/// Defaults when the file doesn't exist; any other problem with it is an error, so a typo
/// doesn't silently leave the old prefix in place.
pub fn load(path: &Path) -> Result<Config> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(anyhow!(e).context(format!("reading {}", path.display()))),
    };
    let parse = || -> Result<Config> {
        let file: File = toml::from_str(&text)?;
        Ok(Config {
            prefix: file
                .prefix
                .as_deref()
                .map(Prefix::parse)
                .transpose()?
                .unwrap_or_default(),
        })
    };
    parse().with_context(|| format!("in {}", path.display()))
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn prefix(s: &str) -> Prefix {
        Prefix::parse(s).unwrap()
    }

    fn load_str(text: &str) -> Result<Config> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        load(&path)
    }

    #[test]
    fn missing_file_means_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load(&dir.path().join("nope.toml")).unwrap(),
            Config::default()
        );
        assert_eq!(Config::default().prefix.byte(), 0x02);
    }

    #[test]
    fn prefix_is_read_from_the_file() {
        let config = load_str("prefix = \"C-a\"\n").unwrap();
        assert_eq!(config.prefix.byte(), 0x01);
        assert_eq!(config.prefix.caret(), "^a");
        assert_eq!(config.prefix.name(), "Ctrl-a");
        assert_eq!(load_str("prefix = \"C-Z\"").unwrap().prefix.byte(), 0x1a);
        assert_eq!(load_str("").unwrap(), Config::default());
    }

    #[test]
    fn bad_files_are_errors_that_name_the_file() {
        for text in [
            "prefix = \"a\"",
            "prefix = \"C-1\"",
            "prefix = \"C-[\"",
            "prefix = \"C-m\"",
            "prefix = \"C-ab\"",
            "prefix = 3",
            "prefx = \"C-a\"",
            "not toml",
        ] {
            let err = format!("{:#}", load_str(text).unwrap_err());
            assert!(err.contains("config.toml"), "{text}: {err}");
        }
    }
}
