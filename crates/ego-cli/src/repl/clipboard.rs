//! Host pasteboard adapter for the interactive seat.

use crate::run::PasteboardKind;

#[cfg(target_os = "macos")]
pub(super) fn kind() -> Result<PasteboardKind, String> {
    use objc2_app_kit::NSPasteboard;

    let board = NSPasteboard::generalPasteboard();
    let Some(types) = board.types() else {
        return Ok(PasteboardKind::Empty);
    };
    let names = types
        .iter()
        .map(|name| name.to_string())
        .collect::<Vec<_>>();
    if names
        .iter()
        .any(|name| name == "public.png" || name == "public.tiff")
    {
        Ok(PasteboardKind::Image)
    } else if names.iter().any(|name| name == "public.utf8-plain-text") {
        Ok(PasteboardKind::Text)
    } else {
        Ok(PasteboardKind::Empty)
    }
}

#[cfg(target_os = "macos")]
pub(super) fn image(max_bytes: u64) -> Result<Vec<u8>, String> {
    use objc2_app_kit::NSPasteboard;
    use objc2_foundation::NSString;

    let board = NSPasteboard::generalPasteboard();
    let png = board.dataForType(&NSString::from_str("public.png"));
    let tiff = board.dataForType(&NSString::from_str("public.tiff"));
    let data = png
        .as_ref()
        .or(tiff.as_ref())
        .ok_or_else(|| "pasteboard image is unavailable as PNG or TIFF".to_owned())?;
    let size = data.len() as u64;
    if size > max_bytes {
        return Err(format!(
            "pasteboard image is {size} bytes, above the {max_bytes}-byte limit"
        ));
    }
    if png.is_some() {
        return Ok(data.to_vec());
    }
    let mut clipboard = arboard::Clipboard::new().map_err(|error| error.to_string())?;
    let decoded = clipboard.get_image().map_err(|error| error.to_string())?;
    encode_rgba(
        decoded.width,
        decoded.height,
        decoded.bytes.as_ref(),
        max_bytes,
    )
}

#[cfg(target_os = "macos")]
pub(super) fn text() -> Result<String, String> {
    use objc2_app_kit::NSPasteboard;
    use objc2_foundation::NSString;

    NSPasteboard::generalPasteboard()
        .stringForType(&NSString::from_str("public.utf8-plain-text"))
        .map(|value| value.to_string())
        .ok_or_else(|| "pasteboard text is unavailable".to_owned())
}

#[cfg(any(target_os = "macos", windows))]
fn encode_rgba(
    width: usize,
    height: usize,
    rgba: &[u8],
    max_bytes: u64,
) -> Result<Vec<u8>, String> {
    let width = u32::try_from(width).map_err(|_| "pasteboard image width is too large")?;
    let height = u32::try_from(height).map_err(|_| "pasteboard image height is too large")?;
    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut encoded, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|error| error.to_string())?;
        writer
            .write_image_data(rgba)
            .map_err(|error| error.to_string())?;
    }
    let size = encoded.len() as u64;
    if size > max_bytes {
        return Err(format!(
            "pasteboard image is {size} bytes, above the {max_bytes}-byte limit"
        ));
    }
    Ok(encoded)
}

#[cfg(windows)]
pub(super) fn kind() -> Result<PasteboardKind, String> {
    use clipboard_win::{Clipboard, formats, is_format_avail};
    let _clipboard = Clipboard::new_attempts(5).map_err(|error| error.to_string())?;
    if is_format_avail(formats::CF_DIB) {
        Ok(PasteboardKind::Image)
    } else if is_format_avail(formats::CF_UNICODETEXT) {
        Ok(PasteboardKind::Text)
    } else {
        Ok(PasteboardKind::Empty)
    }
}

#[cfg(windows)]
pub(super) fn image(max_bytes: u64) -> Result<Vec<u8>, String> {
    use clipboard_win::{Clipboard, formats, size};
    {
        let _clipboard = Clipboard::new_attempts(5).map_err(|error| error.to_string())?;
        let bytes = size(formats::CF_DIB).map_err(|error| error.to_string())? as u64;
        if bytes > max_bytes {
            return Err(format!(
                "pasteboard image is {bytes} bytes, above the {max_bytes}-byte limit"
            ));
        }
    }
    let mut clipboard = arboard::Clipboard::new().map_err(|error| error.to_string())?;
    let decoded = clipboard.get_image().map_err(|error| error.to_string())?;
    encode_rgba(
        decoded.width,
        decoded.height,
        decoded.bytes.as_ref(),
        max_bytes,
    )
}

#[cfg(windows)]
pub(super) fn text() -> Result<String, String> {
    clipboard_win::get_clipboard(clipboard_win::formats::Unicode).map_err(|error| error.to_string())
}

#[cfg(all(unix, not(target_os = "macos")))]
enum LinuxBackend {
    Wayland,
    X11,
}

#[cfg(all(unix, not(target_os = "macos")))]
fn linux_backend() -> Result<(LinuxBackend, String), String> {
    use std::process::Command;

    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        if let Ok(output) = Command::new("wl-paste").arg("--list-types").output() {
            if output.status.success() {
                return Ok((
                    LinuxBackend::Wayland,
                    String::from_utf8_lossy(&output.stdout).into_owned(),
                ));
            }
        }
    }
    if std::env::var_os("DISPLAY").is_some() {
        let output = Command::new("xclip")
            .args(["-selection", "clipboard", "-t", "TARGETS", "-o"])
            .output()
            .map_err(|error| format!("xclip could not inspect the pasteboard: {error}"))?;
        if output.status.success() {
            return Ok((
                LinuxBackend::X11,
                String::from_utf8_lossy(&output.stdout).into_owned(),
            ));
        }
    }
    Err("neither wl-paste nor xclip could inspect the OS pasteboard".to_owned())
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(super) fn kind() -> Result<PasteboardKind, String> {
    let (_, types) = linux_backend()?;
    let types = types.to_ascii_lowercase();
    if types.contains("image/png") {
        Ok(PasteboardKind::Image)
    } else if types.contains("text/plain") || types.contains("utf8_string") {
        Ok(PasteboardKind::Text)
    } else {
        Ok(PasteboardKind::Empty)
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn linux_read(mime: &str, max_bytes: u64) -> Result<Vec<u8>, String> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let (backend, _) = linux_backend()?;
    let mut command = match backend {
        LinuxBackend::Wayland => {
            let mut command = Command::new("wl-paste");
            command.args(["--no-newline", "--type", mime]);
            command
        }
        LinuxBackend::X11 => {
            let mut command = Command::new("xclip");
            command.args(["-selection", "clipboard", "-t", mime, "-o"]);
            command
        }
    };
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("pasteboard read could not start: {error}"))?;
    let mut output = Vec::new();
    let mut chunk = [0_u8; 64 * 1024];
    let mut stdout = child.stdout.take().ok_or("pasteboard read has no output")?;
    loop {
        let count = stdout.read(&mut chunk).map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        let observed = output.len() as u64 + count as u64;
        if observed > max_bytes {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "pasteboard image is at least {observed} bytes, above the {max_bytes}-byte limit"
            ));
        }
        output.extend_from_slice(&chunk[..count]);
    }
    let status = child.wait().map_err(|error| error.to_string())?;
    if !status.success() {
        return Err(format!("pasteboard read exited with {status}"));
    }
    Ok(output)
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(super) fn image(max_bytes: u64) -> Result<Vec<u8>, String> {
    linux_read("image/png", max_bytes)
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(super) fn text() -> Result<String, String> {
    String::from_utf8(linux_read("text/plain;charset=utf-8", 64 * 1024 * 1024)?)
        .map_err(|error| error.to_string())
}
