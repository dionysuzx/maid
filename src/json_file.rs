use anyhow::{Context, Result};
use serde::{Serialize, de::DeserializeOwned};
use std::{fs, path::Path};

/// Reads a JSON ledger, treating a missing file as an empty one.
pub fn read<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs::read_to_string(path) {
        Ok(contents) => serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// Replaces a JSON ledger atomically, so a crash leaves the old or the new file.
pub fn write<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;

    let mut file = tempfile::Builder::new()
        .prefix(".maid-ledger.")
        .tempfile_in(parent)
        .with_context(|| format!("failed to create temp ledger in {}", parent.display()))?;

    serde_json::to_writer_pretty(&mut file, value)
        .with_context(|| format!("failed to encode {}", path.display()))?;
    file.as_file()
        .sync_all()
        .with_context(|| format!("failed to sync {}", path.display()))?;
    file.into_temp_path()
        .persist(path)
        .map_err(|err| err.error)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}
