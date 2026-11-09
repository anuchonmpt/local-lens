use crate::kubernetes::config_files;
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

const SCAN_INTERVAL: Duration = Duration::from_secs(4);
type FileStamp = (PathBuf, Option<SystemTime>, u64);

#[derive(Clone, Deserialize, Serialize)]
struct Source {
    path: PathBuf,
    imported: bool,
}

pub struct KubeconfigSettings {
    storage: Option<PathBuf>,
    sources: Vec<Source>,
    files: Vec<FileStamp>,
    last_probe: Option<Instant>,
    path: String,
    yaml: String,
    message: String,
}

impl KubeconfigSettings {
    pub fn load(storage: Option<PathBuf>) -> Self {
        let sources: Vec<Source> = storage
            .as_ref()
            .and_then(|dir| fs::read(dir.join("kubeconfig-sources.json")).ok())
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default();
        Self {
            files: if storage.is_some() {
                file_stamps(&sources)
            } else {
                Vec::new()
            },
            storage,
            sources,
            last_probe: Some(Instant::now()),
            path: String::new(),
            yaml: String::new(),
            message: String::new(),
        }
    }

    pub fn sources(&self) -> Vec<PathBuf> {
        self.sources
            .iter()
            .map(|source| source.path.clone())
            .collect()
    }

    pub fn sync_due(&mut self) -> bool {
        if self
            .last_probe
            .is_some_and(|last| last.elapsed() < SCAN_INTERVAL)
        {
            return false;
        }
        let files = file_stamps(&self.sources);
        let changed = self.last_probe.is_none() || self.files != files;
        self.files = files;
        self.last_probe = Some(Instant::now());
        changed
    }

    pub fn show(&mut self, ui: &mut egui::Ui) {
        ui.heading("Kubernetes");
        ui.separator();
        ui.strong("Manage Kubeconfigs");
        ui.label("Add a kubeconfig or sync a folder to keep Local Kubeconfigs up to date.");

        #[cfg(target_os = "macos")]
        ui.horizontal(|ui| {
            if ui.button("Add Kubeconfig…").clicked() {
                if let Some(file) = rfd::FileDialog::new()
                    .set_title("Add Kubeconfig")
                    .pick_file()
                {
                    self.add(file);
                }
            }
            if ui.button("Sync Folder…").clicked() {
                if let Some(folder) = rfd::FileDialog::new()
                    .set_title("Sync Kubeconfig Folder")
                    .pick_folder()
                {
                    self.add(folder);
                }
            }
        });

        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.path)
                    .hint_text("Kubeconfig file or folder path")
                    .desired_width(360.0),
            );
            if ui
                .add_enabled(!self.path.trim().is_empty(), egui::Button::new("Sync Path"))
                .clicked()
            {
                self.add(PathBuf::from(self.path.trim()));
            }
        });

        ui.collapsing("Paste Kubeconfig", |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut self.yaml)
                    .code_editor()
                    .hint_text("Paste kubeconfig YAML")
                    .desired_width(f32::INFINITY)
                    .desired_rows(5),
            );
            if ui
                .add_enabled(
                    !self.yaml.trim().is_empty(),
                    egui::Button::new("Add Pasted Config"),
                )
                .clicked()
            {
                if let Err(error) = self.import_yaml() {
                    self.message = error;
                }
            }
        });

        ui.separator();
        ui.strong("Synced Items");
        ui.horizontal(|ui| {
            ui.label("Folder");
            ui.monospace("~/.kube");
            ui.label("Default");
        });
        let mut remove = None;
        egui::ScrollArea::vertical()
            .max_height(260.0)
            .show(ui, |ui| {
                for (index, source) in self.sources.iter().enumerate() {
                    let source = &source.path;
                    ui.push_id(index, |ui| {
                        ui.horizontal(|ui| {
                            if ui
                                .small_button("Remove")
                                .on_hover_text("Stop syncing this source")
                                .clicked()
                            {
                                remove = Some(index);
                            }
                            let kind = if source.is_dir() {
                                "Folder"
                            } else if source.is_file() {
                                "File"
                            } else {
                                "Missing"
                            };
                            ui.label(kind);
                            let path = source.to_string_lossy();
                            ui.add(egui::Label::new(path.as_ref()).truncate())
                                .on_hover_text(path.as_ref());
                        });
                    });
                }
            });
        if let Some(index) = remove {
            self.message = self.remove_source(index).unwrap_or_else(|error| error);
        }
        if !self.message.is_empty() {
            ui.label(&self.message);
        }
    }

    pub fn clear_draft(&mut self) {
        self.yaml.clear();
    }

    fn add(&mut self, path: PathBuf) {
        self.message = match self.add_source(path, false) {
            Ok(()) => {
                self.path.clear();
                "Source added. Local Kubeconfigs will refresh automatically.".into()
            }
            Err(error) => error,
        };
    }

    fn add_source(&mut self, path: PathBuf, imported: bool) -> Result<(), String> {
        let path = if let Ok(relative) = path.strip_prefix("~") {
            let home = std::env::var_os("HOME").ok_or("Could not find the home directory.")?;
            PathBuf::from(home).join(relative)
        } else {
            std::path::absolute(path).map_err(|error| error.to_string())?
        };
        if !path.is_file() && !path.is_dir() {
            return Err("Choose an existing kubeconfig file or folder.".into());
        }
        if self.sources.iter().any(|source| source.path == path)
            || default_folder().as_ref() == Some(&path)
        {
            return Err("This source is already synced.".into());
        }
        let mut sources = self.sources.clone();
        sources.push(Source { path, imported });
        self.save(sources)
    }

    fn remove_source(&mut self, index: usize) -> Result<String, String> {
        let mut sources = self.sources.clone();
        let removed = sources.remove(index);
        self.save(sources)?;
        if removed.imported
            && self.storage.as_ref().is_some_and(|dir| {
                removed.path.parent() == Some(dir.join("imported-kubeconfigs").as_path())
            })
        {
            if let Err(error) = fs::remove_file(removed.path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    return Err(format!(
                        "Stopped syncing. Could not delete the imported copy: {error}"
                    ));
                }
            }
        }
        Ok("Stopped syncing the selected item.".into())
    }

    fn import_yaml(&mut self) -> Result<(), String> {
        let config: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(&self.yaml).map_err(|_| "Paste valid kubeconfig YAML.")?;
        if config["kind"].as_str() != Some("Config")
            || config["contexts"].as_sequence().is_none_or(Vec::is_empty)
        {
            return Err("The kubeconfig must contain at least one context.".into());
        }
        let dir = self
            .storage
            .as_ref()
            .ok_or("Settings storage is unavailable.")?
            .join("imported-kubeconfigs");
        fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        let mut file = tempfile::Builder::new()
            .prefix("config-")
            .suffix(".yaml")
            .tempfile_in(dir)
            .map_err(|error| error.to_string())?;
        file.write_all(self.yaml.as_bytes())
            .map_err(|error| error.to_string())?;
        let (_, path) = file.keep().map_err(|error| error.to_string())?;
        if let Err(error) = self.add_source(path.clone(), true) {
            let _ = fs::remove_file(path);
            return Err(error);
        }
        self.yaml.clear();
        self.message = "Kubeconfig added.".into();
        Ok(())
    }

    fn save(&mut self, sources: Vec<Source>) -> Result<(), String> {
        let dir = self
            .storage
            .as_ref()
            .ok_or("Settings storage is unavailable.")?;
        let result = (|| -> std::io::Result<()> {
            fs::create_dir_all(dir)?;
            let mut file = tempfile::NamedTempFile::new_in(dir)?;
            serde_json::to_writer_pretty(file.as_file_mut(), &sources)?;
            file.persist(dir.join("kubeconfig-sources.json"))?;
            Ok(())
        })();
        result.map_err(|error| format!("Could not save synced items: {error}"))?;
        self.sources = sources;
        self.last_probe = None;
        Ok(())
    }
}

fn default_folder() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| Path::new(&home).join(".kube"))
}

fn file_stamps(sources: &[Source]) -> Vec<FileStamp> {
    let default = default_folder();
    let kubeconfig = std::env::var_os("KUBECONFIG");
    let extra = kubeconfig
        .as_ref()
        .map(|paths| std::env::split_paths(paths).collect::<Vec<_>>())
        .unwrap_or_default();
    config_files(
        default
            .iter()
            .chain(sources.iter().map(|source| &source.path))
            .chain(&extra),
    )
    .into_iter()
    .filter_map(|path| {
        let metadata = fs::metadata(&path).ok()?;
        Some((path, metadata.modified().ok(), metadata.len()))
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synced_folder_persists_and_detects_file_changes() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().join("settings");
        let folder = root.path().join("configs");
        fs::create_dir(&folder).unwrap();
        let mut settings = KubeconfigSettings::load(Some(storage.clone()));
        settings.add_source(folder.clone(), false).unwrap();

        let mut settings = KubeconfigSettings::load(Some(storage));
        assert_eq!(settings.sources(), vec![folder.clone()]);
        assert!(!settings.sync_due());
        let config = folder.join("team.yaml");
        fs::write(&config, "original").unwrap();
        settings.last_probe = Some(Instant::now() - SCAN_INTERVAL);
        assert!(settings.sync_due());
        fs::write(&config, "updated kubeconfig").unwrap();
        settings.last_probe = Some(Instant::now() - SCAN_INTERVAL);
        assert!(settings.sync_due());
        fs::remove_file(&config).unwrap();
        settings.last_probe = Some(Instant::now() - SCAN_INTERVAL);
        assert!(settings.sync_due());
        fs::write(&config, "external file").unwrap();
        settings.remove_source(0).unwrap();
        assert!(config.exists());
    }
}
