use std::{
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct AppConfig {
    pub default_name: Option<String>,
    pub last_data_dir: Option<String>,
    pub last_invite: Option<String>,
    pub relay_only: bool,
    pub voice_input_device: Option<String>,
    pub voice_output_device: Option<String>,
}

pub fn default_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("grimoire")
        .join("config.json")
}

pub fn default_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("grimoire")
        .join("community")
}

pub fn load(path: &Path) -> AppConfig {
    [path.to_owned(), path.with_extension("json.bak")]
        .into_iter()
        .find_map(|path| {
            std::fs::read_to_string(path)
                .ok()
                .and_then(|content| serde_json::from_str(&content).ok())
        })
        .unwrap_or_default()
}

pub fn save(path: &Path, config: &AppConfig) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    let backup = path.with_extension("json.bak");
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(
        serde_json::to_string_pretty(config)
            .expect("config serializes")
            .as_bytes(),
    )?;
    file.sync_all()?;
    drop(file);
    if path.exists() {
        let _ = std::fs::remove_file(&backup);
        std::fs::rename(path, &backup)?;
    }
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::rename(&backup, path);
        return Err(error);
    }
    let _ = std::fs::remove_file(backup);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrips() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        let config = AppConfig {
            default_name: Some("maren".into()),
            last_data_dir: Some("/tmp/community".into()),
            last_invite: Some("invite".into()),
            relay_only: true,
            voice_input_device: Some("studio mic".into()),
            voice_output_device: Some("headphones".into()),
        };

        save(&path, &config).unwrap();

        assert_eq!(load(&path), config);
    }

    #[test]
    fn missing_or_corrupt_config_uses_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        assert_eq!(load(&path), AppConfig::default());
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load(&path), AppConfig::default());
    }

    #[test]
    fn load_recovers_the_last_complete_backup() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        let backup = path.with_extension("json.bak");
        let config = AppConfig {
            last_data_dir: Some("/tmp/community".into()),
            ..AppConfig::default()
        };
        std::fs::write(&path, "truncated").unwrap();
        std::fs::write(&backup, serde_json::to_string(&config).unwrap()).unwrap();

        assert_eq!(load(&path), config);
    }
}
