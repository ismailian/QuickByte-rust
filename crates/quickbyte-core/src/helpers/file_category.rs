//! Classifies a file by extension into a coarse category, used for UI grouping
//! (the sidebar tree, row icons) and for category folders. It has no effect on
//! download behaviour. Port of `FileCategoryHelper.cs`.

use serde::{Deserialize, Serialize};

/// Serialized by **name** — it is only ever a dictionary key
/// (`DownloadSettings.CategoryFolders`), and System.Text.Json writes enum keys as names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum FileCategory {
    Compressed,
    Documents,
    Music,
    Programs,
    Video,
    Pictures,
    Other,
}

impl FileCategory {
    pub const ALL: [FileCategory; 7] = [
        Self::Compressed,
        Self::Documents,
        Self::Music,
        Self::Programs,
        Self::Video,
        Self::Pictures,
        Self::Other,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Compressed => "Compressed",
            Self::Documents => "Documents",
            Self::Music => "Music",
            Self::Programs => "Programs",
            Self::Video => "Video",
            Self::Pictures => "Pictures",
            Self::Other => "Other",
        }
    }
}

impl std::fmt::Display for FileCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// The category of a file name, by its extension (case-insensitive).
pub fn get_category(file_name: &str) -> FileCategory {
    let ext = match std::path::Path::new(file_name).extension().and_then(|e| e.to_str()) {
        Some(e) => e.to_ascii_lowercase(),
        None => return FileCategory::Other,
    };
    match ext.as_str() {
        "zip" | "rar" | "7z" | "tar" | "gz" => FileCategory::Compressed,
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "csv" => FileCategory::Documents,
        "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" => FileCategory::Music,
        "exe" | "msi" | "dmg" | "apk" | "appimage" => FileCategory::Programs,
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "webm" | "ogv" => FileCategory::Video,
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" => FileCategory::Pictures,
        _ => FileCategory::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_extension_ignoring_case() {
        assert_eq!(get_category("movie.MKV"), FileCategory::Video);
        assert_eq!(get_category("setup.exe"), FileCategory::Programs);
        assert_eq!(get_category("archive.tar.gz"), FileCategory::Compressed);
        assert_eq!(get_category("README"), FileCategory::Other);
        assert_eq!(get_category("file.unknown"), FileCategory::Other);
    }
}
