use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    pins: Vec<String>,
}

fn default_path(xdg: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    let directory = if let Some(xdg) = xdg.filter(|value| !value.is_empty()) {
        PathBuf::from(xdg)
    } else {
        PathBuf::from(
            home.filter(|value| !value.is_empty())
                .context("HOME is not set; cannot locate the default config file")?,
        )
        .join(".config")
    };
    Ok(directory.join("waywire/config.toml"))
}

fn load_pins(path: &Path, explicit: bool, flags: Vec<String>) -> Result<Vec<String>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if !explicit && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(flags);
        }
        Err(error) => return Err(error).with_context(|| format!("read config {}", path.display())),
    };
    let mut config: FileConfig =
        toml::from_str(&contents).with_context(|| format!("parse config {}", path.display()))?;
    config.pins.extend(flags);
    Ok(config.pins)
}

pub(super) fn pins(explicit: Option<&Path>, flags: Vec<String>) -> Result<Vec<String>> {
    let path = match explicit {
        Some(path) => path.to_path_buf(),
        None => default_path(
            std::env::var_os("XDG_CONFIG_HOME"),
            std::env::var_os("HOME"),
        )?,
    };
    load_pins(&path, explicit.is_some(), flags)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_pins_precede_flags_and_keep_shell_quoting() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "pins = [\"foot\", \"xterm -title 'My Window'\"]").expect("config");
        let pins = load_pins(&path, true, vec!["editor --file 'two words'".into()]).expect("pins");
        assert_eq!(
            pins,
            [
                "foot",
                "xterm -title 'My Window'",
                "editor --file 'two words'"
            ]
        );
    }

    #[test]
    fn missing_default_is_silent_but_missing_explicit_is_an_error() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("config.toml");
        assert_eq!(
            load_pins(&path, false, vec!["foot".into()]).expect("default"),
            ["foot"]
        );
        let error = load_pins(&path, true, vec![]).expect_err("explicit config");
        assert!(format!("{error:#}").contains(&path.display().to_string()));
    }

    #[test]
    fn malformed_toml_and_unknown_keys_name_the_path() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("config.toml");
        for contents in ["pins = [", "unknown = true"] {
            std::fs::write(&path, contents).expect("config");
            let error = load_pins(&path, true, vec![]).expect_err("invalid config");
            assert!(format!("{error:#}").contains(&path.display().to_string()));
        }
    }

    #[test]
    fn default_path_prefers_xdg_over_home() {
        assert_eq!(
            default_path(Some("/xdg".into()), Some("/home/user".into())).expect("xdg"),
            PathBuf::from("/xdg/waywire/config.toml")
        );
        assert_eq!(
            default_path(None, Some("/home/user".into())).expect("home"),
            PathBuf::from("/home/user/.config/waywire/config.toml")
        );
    }
}
