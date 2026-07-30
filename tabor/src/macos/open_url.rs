use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

use url::Url;

const LOCAL_FILE_PROBE_BYTES: usize = 64;
const REMOTE_PDF_EXTENSIONS: &[&str] = &["pdf"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenUrlKind {
    Web,
    Image,
    Pdf,
}

pub(crate) fn classify_open_url(url: &str) -> OpenUrlKind {
    if crate::macos::image_view::is_image_source(url) {
        OpenUrlKind::Image
    } else if is_remote_pdf_url(url) || is_local_pdf_url(url) {
        OpenUrlKind::Pdf
    } else {
        OpenUrlKind::Web
    }
}

pub(crate) fn local_file_path(url: &str) -> Option<PathBuf> {
    let parsed = Url::parse(url).ok()?;
    if parsed.scheme() != "file" {
        return None;
    }
    parsed.to_file_path().ok()
}

fn is_remote_pdf_url(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };

    matches!(parsed.scheme(), "http" | "https")
        && parsed
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .and_then(|segment| segment.rsplit_once('.'))
            .is_some_and(|(_, ext)| {
                REMOTE_PDF_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
            })
}

fn is_local_pdf_url(url: &str) -> bool {
    let Some(path) = local_file_path(url) else {
        return false;
    };

    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return false,
    };
    let mut probe = [0u8; LOCAL_FILE_PROBE_BYTES];
    let read = match file.read(&mut probe) {
        Ok(read) => read,
        Err(_) => return false,
    };

    probe[..read].windows(5).any(|window| window == b"%PDF-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_remote_pdf_urls_as_pdf() {
        assert_eq!(
            classify_open_url("https://example.com/files/report.pdf?download=1"),
            OpenUrlKind::Pdf
        );
    }

    #[test]
    fn classifies_local_pdf_urls_by_header_probe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pdf_path = dir.path().join("fixture");
        std::fs::write(&pdf_path, b"%PDF-1.4\n").expect("write pdf");
        let url = Url::from_file_path(&pdf_path).expect("file url").to_string();

        assert_eq!(classify_open_url(&url), OpenUrlKind::Pdf);
    }
}
