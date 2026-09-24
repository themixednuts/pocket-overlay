//! Skins: PNG art that replaces the built-in drawing of the radio's body. Everything that
//! moves (gimbals, switches, LCD bars, labels) is still drawn on top.
//!
//! A skin covers the drawing's frame: 1000 x 1000 units, the same framing as the product
//! photo the drawing was traced from. Any resolution works as long as it's square; the
//! setup page exports a template to paint over.

use std::path::{Path, PathBuf};

/// Big enough for a detailed 4096 x 4096 PNG.
pub const MAX_BYTES: usize = 20 * 1024 * 1024;
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

#[derive(Debug, PartialEq, Eq)]
pub enum SkinError {
    BadName,
    NotPng,
    TooBig,
    Io(String),
}

impl std::fmt::Display for SkinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadName => f.write_str("skin names are 1-40 letters, digits, - or _"),
            Self::NotPng => f.write_str("that file isn't a PNG"),
            Self::TooBig => write!(f, "skins can be at most {} MB", MAX_BYTES / 1024 / 1024),
            Self::Io(e) => write!(f, "couldn't store the skin: {e}"),
        }
    }
}

/// Letters, digits, `-` and `_`, 1-40 long: safe as a file name and in a URL.
pub fn valid_name(name: &str) -> bool {
    (1..=40).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[derive(Debug, Clone)]
pub struct Skins {
    dir: PathBuf,
}

impl Skins {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Next to the settings file: `<settings folder>/skins`.
    pub fn beside(config_path: &Path) -> Self {
        let parent = config_path.parent().unwrap_or(Path::new("."));
        Self::new(parent.join("skins"))
    }

    fn path(&self, name: &str) -> Result<PathBuf, SkinError> {
        if valid_name(name) {
            Ok(self.dir.join(format!("{name}.png")))
        } else {
            Err(SkinError::BadName)
        }
    }

    pub fn list(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let stem = name.strip_suffix(".png")?;
                valid_name(stem).then(|| stem.to_owned())
            })
            .collect();
        names.sort();
        names
    }

    pub fn read(&self, name: &str) -> Option<Vec<u8>> {
        std::fs::read(self.path(name).ok()?).ok()
    }

    pub fn save(&self, name: &str, png: &[u8]) -> Result<(), SkinError> {
        let path = self.path(name)?;
        if png.len() > MAX_BYTES {
            return Err(SkinError::TooBig);
        }
        if !png.starts_with(&PNG_SIGNATURE) {
            return Err(SkinError::NotPng);
        }
        std::fs::create_dir_all(&self.dir).map_err(|e| SkinError::Io(e.to_string()))?;
        std::fs::write(path, png).map_err(|e| SkinError::Io(e.to_string()))
    }

    /// Returns whether there was a skin to remove.
    pub fn remove(&self, name: &str) -> Result<bool, SkinError> {
        match std::fs::remove_file(self.path(name)?) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(SkinError::Io(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52,
    ];

    #[test]
    fn names() {
        assert!(valid_name("carbon-fiber_2"));
        for bad in [
            "",
            "../x",
            "a/b",
            "a.png",
            "con sole",
            &"x".repeat(41),
            "émoji",
        ] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn save_list_read_remove() {
        let dir = tempfile::tempdir().unwrap();
        let skins = Skins::new(dir.path().join("skins"));
        assert!(skins.list().is_empty(), "missing folder is just empty");
        skins.save("b", TINY_PNG).unwrap();
        skins.save("a", TINY_PNG).unwrap();
        assert_eq!(skins.list(), ["a", "b"]);
        assert_eq!(skins.read("a").as_deref(), Some(TINY_PNG));
        assert_eq!(skins.remove("a"), Ok(true));
        assert_eq!(skins.remove("a"), Ok(false));
        assert_eq!(skins.list(), ["b"]);
    }

    #[test]
    fn rejects_bad_input() {
        let dir = tempfile::tempdir().unwrap();
        let skins = Skins::new(dir.path());
        assert_eq!(skins.save("../evil", TINY_PNG), Err(SkinError::BadName));
        assert_eq!(skins.save("ok", b"GIF89a..."), Err(SkinError::NotPng));
        let mut huge = TINY_PNG.to_vec();
        huge.resize(MAX_BYTES + 1, 0);
        assert_eq!(skins.save("ok", &huge), Err(SkinError::TooBig));
        assert!(skins.read("../overlay").is_none());
        assert_eq!(skins.remove("../overlay"), Err(SkinError::BadName));
    }
}
