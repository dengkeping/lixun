use mime_guess::Mime;

pub fn mime_to_icon_name(mime: &Mime) -> String {
    match mime.essence_str() {
        "application/pdf" => "application-pdf".into(),
        "text/plain" => "text-plain".into(),
        "text/markdown" | "text/x-markdown" => "text-x-markdown".into(),
        "application/zip" | "application/x-tar" | "application/x-gzip" => {
            "package-x-generic".into()
        }
        "inode/directory" => "folder".into(),
        _ if mime.type_() == mime_guess::mime::IMAGE => "image-x-generic".into(),
        _ if mime.type_() == mime_guess::mime::VIDEO => "video-x-generic".into(),
        _ if mime.type_() == mime_guess::mime::AUDIO => "audio-x-generic".into(),
        _ => "text-x-generic".into(),
    }
}

pub fn human_kind(mime: &Mime) -> String {
    match mime.essence_str() {
        "application/pdf" => "PDF Document".into(),
        "text/plain" => "Text".into(),
        "text/markdown" | "text/x-markdown" => "Markdown".into(),
        "application/json" => "JSON".into(),
        "application/zip" | "application/x-tar" | "application/x-gzip" => "Archive".into(),
        "inode/directory" => "Folder".into(),
        _ if mime.type_() == mime_guess::mime::IMAGE => "Image".into(),
        _ if mime.type_() == mime_guess::mime::VIDEO => "Video".into(),
        _ if mime.type_() == mime_guess::mime::AUDIO => "Audio".into(),
        "application/octet-stream" => "File".into(),
        _ => gio::content_type_from_mime_type(mime.essence_str())
            .map(|kind| gio::content_type_get_description(&kind))
            .filter(|label| !label.is_empty() && !label.contains('/'))
            .map(|label| label.to_string())
            .unwrap_or_else(|| "File".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mime(input: &str) -> Mime {
        input.parse().unwrap()
    }

    #[test]
    fn test_mime_to_icon_name_examples() {
        assert_eq!(
            mime_to_icon_name(&mime("application/pdf")),
            "application-pdf"
        );
        assert_eq!(mime_to_icon_name(&mime("text/plain")), "text-plain");
        assert_eq!(mime_to_icon_name(&mime("text/markdown")), "text-x-markdown");
        assert_eq!(
            mime_to_icon_name(&mime("text/x-markdown")),
            "text-x-markdown"
        );
        assert_eq!(mime_to_icon_name(&mime("image/png")), "image-x-generic");
        assert_eq!(mime_to_icon_name(&mime("image/jpeg")), "image-x-generic");
        assert_eq!(mime_to_icon_name(&mime("video/mp4")), "video-x-generic");
        assert_eq!(mime_to_icon_name(&mime("audio/mpeg")), "audio-x-generic");
        assert_eq!(
            mime_to_icon_name(&mime("application/zip")),
            "package-x-generic"
        );
        assert_eq!(
            mime_to_icon_name(&mime("application/x-tar")),
            "package-x-generic"
        );
        assert_eq!(
            mime_to_icon_name(&mime("application/x-gzip")),
            "package-x-generic"
        );
        assert_eq!(mime_to_icon_name(&mime("inode/directory")), "folder");
        assert_eq!(
            mime_to_icon_name(&mime("application/octet-stream")),
            "text-x-generic"
        );
    }

    #[test]
    fn test_human_kind_examples() {
        assert_eq!(human_kind(&mime("application/pdf")), "PDF Document");
        assert_eq!(human_kind(&mime("text/plain")), "Text");
        assert_eq!(human_kind(&mime("text/markdown")), "Markdown");
        assert_eq!(human_kind(&mime("text/x-markdown")), "Markdown");
        assert_eq!(human_kind(&mime("image/png")), "Image");
        assert_eq!(human_kind(&mime("video/mp4")), "Video");
        assert_eq!(human_kind(&mime("audio/mpeg")), "Audio");
        assert_eq!(human_kind(&mime("application/json")), "JSON");
        assert_eq!(human_kind(&mime("application/zip")), "Archive");
        assert_eq!(human_kind(&mime("application/x-tar")), "Archive");
        assert_eq!(human_kind(&mime("inode/directory")), "Folder");
        assert_eq!(human_kind(&mime("application/octet-stream")), "File");
        assert_eq!(human_kind(&mime("application/x-lixun-unknown")), "File");
        for ext in [
            "xlsx", "docx", "pptx", "ods", "odt", "csv", "html", "xml", "7z",
        ] {
            let mime = mime_guess::from_ext(ext).first_or_octet_stream();
            let label = human_kind(&mime);
            assert_ne!(label, "File", "{ext}");
            assert!(!label.to_lowercase().contains("vnd"), "{ext}: {label}");
            assert!(!label.contains('/'), "{ext}: {label}");
        }
    }
}
