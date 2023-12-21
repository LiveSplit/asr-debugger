use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use bstr::ByteSlice;
use livesplit_auto_splitting::settings::FileFilter;

pub fn build(filters: Arc<Vec<FileFilter>>) -> egui_file::Filter<PathBuf> {
    Box::new(move |p: &Path| {
        let name = p.file_name().unwrap_or_default().as_encoded_bytes();
        filters.iter().any(|filter| matches_filter(name, filter))
    })
}

fn matches_filter(file_name: &[u8], filter: &FileFilter) -> bool {
    match filter {
        FileFilter::Name {
            description: _,
            pattern,
        } => pattern
            .split(' ')
            .any(|pattern| matches_single_pattern(file_name, pattern.as_bytes())),
        FileFilter::MimeType(mime_type) => matches_mime_type(file_name, mime_type),
    }
}

fn matches_single_pattern(mut file_name: &[u8], mut pattern: &[u8]) -> bool {
    let mut strip_any = false;
    while !pattern.is_empty() {
        strip_any = if let [b'*', rem @ ..] = pattern {
            pattern = rem;
            true
        } else {
            let (fixed, rem) = pattern.split_at(
                pattern
                    .iter()
                    .position(|&b| b == b'*')
                    .unwrap_or(pattern.len()),
            );
            pattern = rem;
            file_name = if strip_any {
                let Some((_, rem)) = file_name.split_once_str(fixed) else {
                    return false;
                };
                rem
            } else {
                let Some(rem) = file_name.strip_prefix(fixed.as_bytes()) else {
                    return false;
                };
                rem
            };
            false
        };
    }
    strip_any || file_name.is_empty()
}

fn matches_mime_type(file_name: &[u8], mime_type: &str) -> bool {
    let Some((top, sub)) = mime_type.split_once('/') else {
        return false;
    };
    let Some(extensions) = mime_guess::get_extensions(top, sub) else {
        return false;
    };
    let Some((_, extension)) = file_name.rsplit_once_str(&[b'.']) else {
        return false;
    };
    extensions.iter().any(|ext| extension == ext.as_bytes())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_matches_single_pattern() {
        assert!(matches_single_pattern(b"bar.exe", b"*.exe"));
        assert!(!matches_single_pattern(b"bar.exeafter", b"*.exe"));
        assert!(!matches_single_pattern(b"beforebar.exe", b"bar*"));
        assert!(matches_single_pattern(b"beforebarafter", b"*bar*"));
        assert!(matches_single_pattern(b"bar.txt", b"*.txt"));
        assert!(matches_single_pattern(b"quick brown fox", b"*ick*row*ox"));
        assert!(matches_single_pattern(b"quick brown fox", b"q*ick*row*ox"));
        assert!(!matches_single_pattern(b"quick brown fox", b"*row*ox*ick*"));
    }

    #[test]
    fn test_matches_mime_type() {
        assert!(matches_mime_type(b"foo.txt", "text/plain"));
        assert!(matches_mime_type(b"foo.jpg", "image/jpeg"));
        assert!(matches_mime_type(b"foo.jpeg", "image/jpeg"));
        assert!(matches_mime_type(b"foo.png", "image/png"));

        assert!(!matches_mime_type(b"foo.txt", "image/*"));
        assert!(matches_mime_type(b"foo.jpg", "image/*"));
        assert!(matches_mime_type(b"foo.jpeg", "image/*"));
        assert!(matches_mime_type(b"foo.png", "image/*"));

        assert!(!matches_mime_type(b"txt", "text/plain"));
        assert!(!matches_mime_type(b"jpg", "image/jpeg"));
        assert!(!matches_mime_type(b"jpeg", "image/jpeg"));
        assert!(!matches_mime_type(b"png", "image/png"));

        assert!(!matches_mime_type(b"footxt", "text/plain"));
        assert!(!matches_mime_type(b"foojpg", "image/jpeg"));
        assert!(!matches_mime_type(b"foojpeg", "image/jpeg"));
        assert!(!matches_mime_type(b"foopng", "image/png"));

        assert!(!matches_mime_type(b"foo.txt", "image/jpeg"));
        assert!(!matches_mime_type(b"foo.jpg", "image/png"));
        assert!(!matches_mime_type(b"foo.jpeg", "image/png"));
        assert!(!matches_mime_type(b"foo.png", "text/plain"));
    }
}
