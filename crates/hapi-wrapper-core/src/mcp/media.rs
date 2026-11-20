use crate::{rpc::handlers::GeneratedImages, session::client::SessionClient};
use serde_json::{json, Value};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{
    fs::OpenOptions,
    io::{self, Read},
    path::Path,
};
use uuid::Uuid;

fn image(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if b.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else if b.starts_with(&[0, 0, 0])
        && b.get(4..8) == Some(b"ftyp")
        && matches!(b.get(8..12), Some(b"avif" | b"avis"))
    {
        Some("image/avif")
    } else {
        None
    }
}

fn video(b: &[u8]) -> Option<&'static str> {
    if b.get(4..8) == Some(b"ftyp")
        && matches!(
            b.get(8..12),
            Some(
                b"isom"
                    | b"iso2"
                    | b"iso3"
                    | b"iso4"
                    | b"iso5"
                    | b"iso6"
                    | b"mp41"
                    | b"mp42"
                    | b"mp71"
                    | b"avc1"
                    | b"avc3"
                    | b"hev1"
                    | b"hvc1"
                    | b"mmp4"
                    | b"dash"
                    | b"msnv"
                    | b"ndas"
                    | b"ndsc"
                    | b"ndsh"
                    | b"ndsm"
                    | b"ndsp"
                    | b"ndss"
                    | b"ndxc"
                    | b"ndxh"
                    | b"ndxm"
                    | b"ndxp"
                    | b"ndxs"
            )
        )
    {
        return Some("video/mp4");
    }
    if b.starts_with(b"\x1a\x45\xdf\xa3") {
        let header = &b[..b.len().min(128)];
        for i in 4..header.len().saturating_sub(3) {
            if header[i..].starts_with(b"\x42\x82") && header[i + 2] > 128 {
                if let Some(doc) = header.get(i + 3..i + 3 + (header[i + 2] & 127) as usize) {
                    return (doc == b"webm").then_some("video/webm");
                }
            }
        }
    }
    None
}

fn tracks(mut b: &[u8], depth: usize, found: &mut [bool; 2]) -> Option<()> {
    while !b.is_empty() {
        let mut size = u32::from_be_bytes(b.get(..4)?.try_into().ok()?) as usize;
        let kind = b.get(4..8)?;
        let mut header = 8;
        match size {
            0 => size = b.len(),
            1 => {
                size = usize::try_from(u64::from_be_bytes(b.get(8..16)?.try_into().ok()?)).ok()?;
                header = 16
            }
            _ => {}
        }
        let payload = b.get(header..size)?;
        if depth < 3 && kind == [b"moov", b"trak", b"mdia"][depth] {
            tracks(payload, depth + 1, found)?;
        }
        if depth == 3 && kind == b"hdlr" {
            let handler = payload.get(8..12)?;
            found[0] |= handler == b"vide";
            found[1] |= handler == b"soun";
        }
        b = &b[size..];
    }
    Some(())
}

fn media(b: &[u8]) -> &'static str {
    if let Some(i) = image(b) {
        return i;
    }
    if b.len() >= 12 && b.get(4..8) == Some(b"ftyp") {
        let mut found = [false; 2];
        if tracks(b, 0, &mut found).is_some() {
            if found[0] {
                return "video/mp4";
            }
            if found[1] {
                return "audio/mp4";
            }
        }
        return if matches!(b.get(8..12), Some(b"M4A " | b"M4B " | b"M4P ")) {
            "audio/mp4"
        } else {
            "application/octet-stream"
        };
    }
    if let Some(v) = video(b) {
        return v;
    }
    if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WAVE") {
        return "audio/wav";
    }
    if b.starts_with(b"fLaC") {
        return "audio/flac";
    }
    if b.len() >= 32
        && b.starts_with(b"OggS")
        && [b"OpusHead".as_slice(), b"vorbis", b"Speex"]
            .iter()
            .any(|s| b[..b.len().min(96)].windows(s.len()).any(|w| w == *s))
    {
        return "audio/ogg";
    }
    if b.starts_with(b"ID3")
        || (b.len() >= 3
            && b[0] == 255
            && b[1] & 224 == 224
            && b[1] & 24 != 8
            && b[1] & 6 != 0
            && !matches!(b[2] & 240, 0 | 240)
            && b[2] & 12 != 12)
    {
        return "audio/mpeg";
    }
    "application/octet-stream"
}

fn read(path: &Path) -> io::Result<Vec<u8>> {
    #[cfg(not(unix))]
    {
        if !std::fs::metadata(path)?.is_file() {
            return Err(io::Error::other("Path is not a regular file"));
        }
    }
    let mut open = OpenOptions::new();
    open.read(true);
    #[cfg(unix)]
    open.custom_flags(nix::libc::O_NONBLOCK);
    let mut file = open.open(path)?;
    let stat = file.metadata()?;
    if !stat.is_file() {
        return Err(io::Error::other("Path is not a regular file"));
    }
    if stat.len() > 25 << 20 {
        return Err(io::Error::other("File is too large to display inline"));
    }
    let mut bytes = vec![0; stat.len() as usize];
    file.read_exact(&mut bytes).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            io::Error::other("File changed while reading")
        } else {
            e
        }
    })?;
    if file.metadata()?.len() != stat.len() {
        return Err(io::Error::other("File changed while reading"));
    }
    Ok(bytes)
}

fn register_media(
    images: &GeneratedImages,
    tool: &str,
    args: &Value,
) -> Result<(String, String, &'static str), String> {
    let path = Path::new(args["path"].as_str().unwrap());
    let bytes = read(path).map_err(|e| e.to_string())?;
    let mime = match tool {
        "display_image" => image(&bytes).ok_or("Unsupported image content")?,
        "display_video" => video(&bytes).ok_or("Unsupported video content")?,
        _ => media(&bytes),
    };
    let id = Uuid::new_v4().to_string();
    let fallback = format!("{id}.png");
    let mut name = args["title"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| path.file_name()?.to_str())
        .unwrap_or(&fallback)
        .to_owned();
    if Path::new(&name).extension().is_none() {
        if let Some(ext) = path.extension() {
            name.push('.');
            name.push_str(&ext.to_string_lossy());
        }
    }
    images.insert(id.clone(), bytes, mime.into(), name.clone())?;
    Ok((id, name, mime))
}

pub(super) fn display(
    client: &SessionClient,
    images: &GeneratedImages,
    tool: &str,
    args: &Value,
) -> Result<String, String> {
    let (id, name, mime) = register_media(images, tool, args)?;
    client.send_agent_message(
        json!({"type":"generated-image","imageId":id,"fileName":name,"mimeType":mime,"id":Uuid::new_v4().to_string(),"source":{"ingress":"mcp","toolName":tool}}),
        None,
    );
    Ok(format!("Displayed {}: {name}", &tool[8..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::{handlers::files, manager::RpcHandlerManager};

    #[test]
    fn media_signatures_and_regular_files() {
        assert_eq!(image(b"GIF89a"), Some("image/gif"));
        assert_eq!(media(b"RIFF1234WAVE"), "audio/wav");
        assert_eq!(
            video(b"\x1a\x45\xdf\xa3\x42\x82\x84webm"),
            Some("video/webm")
        );
        assert_eq!(video(b"\x1a\x45\xdf\xa3\x42\x82\x88matroska"), None);
        assert_eq!(
            media(b"\x00\x00\x00\x14ftypisom"),
            "application/octet-stream"
        );
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            read(tmp.path()).unwrap_err().to_string(),
            "Path is not a regular file"
        );
    }

    #[tokio::test]
    async fn registers_into_shared_generated_images_store() {
        let tmp = tempfile::tempdir().unwrap();
        let png_path = tmp.path().join("shot.png");
        let png = b"\x89PNG\r\n\x1a\nrest";
        std::fs::write(&png_path, png).unwrap();
        let images = GeneratedImages::default();
        let (id, name, mime) = register_media(
            &images,
            "display_image",
            &json!({"path": png_path, "title": "Preview"}),
        )
        .unwrap();
        assert_eq!(name, "Preview.png");
        assert_eq!(mime, "image/png");

        let rpc = RpcHandlerManager::new("s");
        files::register(&rpc, tmp.path(), &images);
        let raw = rpc
            .handle_request(
                &json!({"method": "s:readGeneratedImage", "params": json!({"id": id}).to_string()}),
            )
            .await
            .unwrap();
        let reply: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(reply["success"], true);
        assert_eq!(reply["mimeType"], "image/png");
        assert_eq!(reply["fileName"], "Preview.png");

        let txt_path = tmp.path().join("notes.txt");
        std::fs::write(&txt_path, b"plain text").unwrap();
        assert_eq!(
            register_media(&images, "display_image", &json!({"path": txt_path})).unwrap_err(),
            "Unsupported image content"
        );
        let (_, media_name, media_mime) =
            register_media(&images, "display_media", &json!({"path": txt_path})).unwrap();
        assert_eq!(media_name, "notes.txt");
        assert_eq!(media_mime, "application/octet-stream");
    }
}
