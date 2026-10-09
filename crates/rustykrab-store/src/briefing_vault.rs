//! Bounded dated Markdown notes at an operator-selected root.
use chrono::NaiveDate;
use rustykrab_core::{Error, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};
const LIMIT: usize = 1024 * 1024;
#[derive(Debug, Serialize)]
pub struct BriefingNote {
    pub date: String,
    pub path: String,
    pub bytes: usize,
    pub sha256: String,
    pub content: String,
}
pub struct BriefingVault {
    root: PathBuf,
    guard: Mutex<()>,
}
fn fail() -> Error {
    Error::ToolExecution("Managed briefing vault is unavailable or the note is invalid".into())
}
fn directory(path: &Path) -> Result<()> {
    for part in path.ancestors() {
        let m = fs::symlink_metadata(part).map_err(|_| fail())?;
        if !m.is_dir() || m.file_type().is_symlink() {
            return Err(fail());
        }
    }
    Ok(())
}
fn valid(date: &str) -> bool {
    date.len() == 10
        && NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .is_ok_and(|d| d.format("%Y-%m-%d").to_string() == date)
}
impl BriefingVault {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        if !root.is_absolute()
            || root
                .components()
                .any(|p| matches!(p, std::path::Component::ParentDir))
        {
            return Err(fail());
        }
        for part in root.ancestors() {
            if let Ok(m) = fs::symlink_metadata(part) {
                if !m.is_dir() || m.file_type().is_symlink() {
                    return Err(fail());
                }
            }
        }
        fs::create_dir_all(root).map_err(|_| fail())?;
        directory(root)?;
        private(root)?;
        let folder = root.join("Daily Briefings");
        if fs::symlink_metadata(&folder).is_ok() {
            directory(&folder)?;
        } else {
            fs::create_dir(&folder).map_err(|_| fail())?;
        }
        private(&folder)?;
        Ok(Self {
            root: folder,
            guard: Mutex::new(()),
        })
    }
    pub fn date_from_path(path: &str) -> Result<&str> {
        let date = path
            .strip_prefix("Daily Briefings/Briefing_")
            .and_then(|s| s.strip_suffix(".md"))
            .ok_or_else(fail)?;
        if !valid(date) {
            return Err(fail());
        }
        Ok(date)
    }
    fn path(&self, date: &str) -> Result<PathBuf> {
        if !valid(date) {
            return Err(fail());
        }
        directory(&self.root)?;
        let path = self.root.join(format!("Briefing_{date}.md"));
        if let Ok(m) = fs::symlink_metadata(&path) {
            if !m.is_file() || m.file_type().is_symlink() {
                return Err(fail());
            }
        }
        Ok(path)
    }
    fn read_inner(&self, date: &str) -> Result<BriefingNote> {
        let f = fs::File::open(self.path(date)?)
            .map_err(|_| Error::NotFound("Briefing note".into()))?;
        if f.metadata().map_err(|_| fail())?.len() > LIMIT as u64 {
            return Err(fail());
        }
        let mut content = String::new();
        f.take((LIMIT + 1) as u64)
            .read_to_string(&mut content)
            .map_err(|_| fail())?;
        if content.len() > LIMIT {
            return Err(fail());
        }
        Ok(BriefingNote {
            date: date.into(),
            path: format!("Daily Briefings/Briefing_{date}.md"),
            bytes: content.len(),
            sha256: format!("{:x}", Sha256::digest(content.as_bytes())),
            content,
        })
    }
    pub fn read(&self, date: &str) -> Result<BriefingNote> {
        let _guard = self.guard.lock().map_err(|_| fail())?;
        self.read_inner(date)
    }
    pub fn write(&self, date: &str, content: &str, append: bool) -> Result<BriefingNote> {
        let _guard = self.guard.lock().map_err(|_| fail())?;
        let path = self.path(date)?;
        let body = if append {
            self.read_inner(date)?.content + content
        } else {
            content.into()
        };
        if body.len() > LIMIT {
            return Err(fail());
        }
        let temp = self
            .root
            .join(format!(".briefing-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut f = options.open(&temp).map_err(|_| fail())?;
            f.write_all(body.as_bytes())
                .and_then(|_| f.sync_all())
                .map_err(|_| fail())?;
            self.path(date)?;
            fs::rename(&temp, path).map_err(|_| fail())?;
            self.read_inner(date)
        })();
        let _ = fs::remove_file(temp);
        result
    }
    pub fn list(&self) -> Result<Vec<serde_json::Value>> {
        let _guard = self.guard.lock().map_err(|_| fail())?;
        directory(&self.root)?;
        let mut notes = vec![];
        for (index, entry) in fs::read_dir(&self.root).map_err(|_| fail())?.enumerate() {
            if index >= 3650 {
                return Err(fail());
            }
            let entry = entry.map_err(|_| fail())?;
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(date) = name
                .strip_prefix("Briefing_")
                .and_then(|s| s.strip_suffix(".md"))
            else {
                continue;
            };
            if !valid(date) {
                continue;
            }
            let m = fs::metadata(self.path(date)?).map_err(|_| fail())?;
            if m.len() > LIMIT as u64 {
                return Err(fail());
            }
            notes.push(serde_json::json!({"date":date,"path":format!("Daily Briefings/Briefing_{date}.md"),"bytes":m.len()}));
        }
        notes.sort_by(|a, b| b["date"].as_str().cmp(&a["date"].as_str()));
        Ok(notes)
    }
}
fn private(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|_| fail())?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn round_trip_append_hash_limits_and_permissions() {
        let d = tempfile::tempdir().unwrap();
        let v = BriefingVault::open(d.path().canonicalize().unwrap().join("vault")).unwrap();
        let n = v.write("2026-10-09", "# Email\nprivate", false).unwrap();
        assert_eq!(v.read("2026-10-09").unwrap().sha256, n.sha256);
        assert_eq!(
            v.write("2026-10-09", "\nCalendar", true).unwrap().content,
            "# Email\nprivate\nCalendar"
        );
        assert!(v
            .write("2026-10-09", &"x".repeat(LIMIT + 1), false)
            .is_err());
        assert!(v.read("2026-10-09").unwrap().content.contains("Calendar"));
        assert_eq!(v.list().unwrap().len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(v.path("2026-10-09").unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
    #[test]
    fn traversal_and_invalid_dates_refused() {
        for p in [
            "../secret",
            "Daily Briefings/Briefing_2026-02-30.md",
            "Daily Briefings/Briefing_2026-10-09.md/../x",
            "/Daily Briefings/Briefing_2026-10-09.md",
        ] {
            assert!(BriefingVault::date_from_path(p).is_err());
        }
        assert_eq!(
            BriefingVault::date_from_path("Daily Briefings/Briefing_2026-10-09.md").unwrap(),
            "2026-10-09"
        );
    }
    #[cfg(unix)]
    #[test]
    fn symlink_roots_folders_notes_refused() {
        use std::os::unix::fs::symlink;
        let d = tempfile::tempdir().unwrap();
        let outside = d.path().canonicalize().unwrap().join("outside");
        fs::create_dir(&outside).unwrap();
        let link = d.path().canonicalize().unwrap().join("link");
        symlink(&outside, &link).unwrap();
        assert!(BriefingVault::open(link.join("new")).is_err());
        assert!(!outside.join("new").exists());
        let v = BriefingVault::open(d.path().canonicalize().unwrap().join("vault")).unwrap();
        let external = outside.join("private");
        fs::write(&external, "untouched").unwrap();
        symlink(&external, v.root.join("Briefing_2026-10-09.md")).unwrap();
        assert!(v.read("2026-10-09").is_err());
        assert!(v.write("2026-10-09", "overwrite", false).is_err());
        assert!(v.list().is_err());
        assert_eq!(fs::read_to_string(&external).unwrap(), "untouched");
        fs::remove_file(v.root.join("Briefing_2026-10-09.md")).unwrap();
        fs::remove_dir(&v.root).unwrap();
        symlink(&outside, &v.root).unwrap();
        assert!(v.write("2026-10-09", "overwrite", false).is_err());
    }
}
