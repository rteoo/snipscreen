//! Windows notifications for new snips, the way Snipping Tool does it: a toast with the snip as a
//! preview that opens the snip in an editor when clicked.
//!
//! An unpackaged app can raise toasts once its AppUserModelID is registered under
//! `HKCU\Software\Classes\AppUserModelId`. The toast's click is a `snipscreen:` link (protocol
//! activation), so it works while the toast is up and later from the notification centre, even
//! after SnipScreen was closed: Windows starts `snipscreen.exe --open-link "snipscreen:open?..."`, which hands
//! the link to the running copy (`tray::forward`) or opens the snip itself.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use eframe::egui::{Color32, ColorImage};
use windows::core::HSTRING;
use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{NotificationSetting, ToastNotification, ToastNotificationManager};

use crate::win;

pub const AUMID: &str = "SnipScreen.App";
const AUMID_KEY: &str = "Software\\Classes\\AppUserModelId\\SnipScreen.App";
const PROTOCOL_KEY: &str = "Software\\Classes\\snipscreen";
const SCHEME: &str = "snipscreen:";
/// Snips (and their previews) kept in `recent` for clicks on older notifications.
const RECENT_FILES: usize = 20;
/// Longest side of the picture in the toast; Windows shows it at most ~364 px wide at 200 %.
const PREVIEW_MAX: usize = 728;

/// The PNG behind the toast's app icon and name.
const ICON_PNG: &[u8] = include_bytes!("../assets/snipscreen-icon-256.png");

#[link(name = "shell32")]
extern "system" {
    fn SetCurrentProcessExplicitAppUserModelID(id: *const u16) -> i32;
}

/// Tags this process with `AUMID`, so its windows group with the Start-menu shortcut that carries
/// the same ID and its notifications are attributed to it. Must run before any window exists.
pub fn set_process_id() {
    let id: Vec<u16> = AUMID.encode_utf16().chain(Some(0)).collect();
    unsafe { SetCurrentProcessExplicitAppUserModelID(id.as_ptr()) };
}

/// `%LOCALAPPDATA%\SnipScreen`.
pub fn local_dir() -> Option<PathBuf> {
    win::local_app_data_dir().map(|d| d.join("SnipScreen"))
}

/// Where notification previews, and snips that weren't auto-saved, are kept for a while.
pub fn recent_dir() -> Option<PathBuf> {
    local_dir().map(|d| d.join("recent"))
}

/// Registers what toasts and their clicks need: the AppUserModelID (name and icon) and the
/// `snipscreen:` link handler pointing at this exe. Writes only what differs; cheap when current.
pub fn register(exe: &Path) -> Result<(), String> {
    let dir = local_dir().ok_or("couldn't find your local AppData folder")?;
    let icon = dir.join("icon.png");
    if std::fs::metadata(&icon).map(|m| m.len()).ok() != Some(ICON_PNG.len() as u64) {
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        std::fs::write(&icon, ICON_PNG).map_err(|e| format!("{}: {e}", icon.display()))?;
    }
    let exe = exe.display();
    let wanted = [
        (AUMID_KEY.to_string(), "DisplayName", "SnipScreen".to_string()),
        (AUMID_KEY.to_string(), "IconUri", icon.display().to_string()),
        (PROTOCOL_KEY.to_string(), "", "URL:SnipScreen".to_string()),
        (PROTOCOL_KEY.to_string(), "URL Protocol", String::new()),
        (format!("{PROTOCOL_KEY}\\DefaultIcon"), "", format!("\"{exe}\",0")),
        (format!("{PROTOCOL_KEY}\\shell\\open\\command"), "", open_command(&exe.to_string())),
    ];
    for (key, name, value) in &wanted {
        if win::reg_get(key, name).as_deref() != Some(value.as_str()) {
            win::reg_set(key, name, value)?;
        }
    }
    Ok(())
}

/// The command Windows runs for a `snipscreen:` link. The link goes last, after a flag that tells
/// `main` to read nothing after it as a flag, since not every launcher escapes quotes in it.
fn open_command(exe: &str) -> String {
    format!("\"{exe}\" {} \"%1\"", crate::tray::OPEN_LINK_FLAG)
}

/// One notification.
#[derive(Debug, PartialEq)]
pub struct Toast {
    pub title: String,
    pub lines: Vec<String>,
    /// A PNG to show under the text.
    pub image: Option<PathBuf>,
    /// The `snipscreen:` link opened by a click.
    pub launch: String,
}

impl Toast {
    pub fn xml(&self) -> String {
        let mut texts = format!("<text>{}</text>", escape(&self.title));
        // ToastGeneric shows the title and at most two more lines.
        for line in self.lines.iter().take(2) {
            texts.push_str(&format!("<text>{}</text>", escape(line)));
        }
        let image = self.image.as_ref().map_or(String::new(), |p| format!("<image src=\"{}\"/>", escape(&file_uri(p))));
        format!(
            "<toast activationType=\"protocol\" launch=\"{}\"><visual><binding template=\"ToastGeneric\">{texts}{image}</binding></visual></toast>",
            escape(&self.launch)
        )
    }

    /// Hands the toast to Windows. Blocks for a few milliseconds; call it off the UI thread.
    pub fn show(&self) -> Result<(), String> {
        let fail = |what: &str, e: windows::core::Error| format!("couldn't {what}: {}", e.message());
        let doc = XmlDocument::new().map_err(|e| fail("prepare the notification", e))?;
        doc.LoadXml(&HSTRING::from(self.xml())).map_err(|e| fail("prepare the notification", e))?;
        let toast = ToastNotification::CreateToastNotification(&doc).map_err(|e| fail("create the notification", e))?;
        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID)).map_err(|e| fail("reach Windows notifications", e))?;
        // `Show` succeeds even when the person turned SnipScreen's notifications off, and then nothing
        // appears. Asking first catches that. Windows only knows an unpackaged app's setting after
        // its first notification, and says "element not found" until then, which is no reason to stop.
        match notifier.Setting() {
            Ok(NotificationSetting::Enabled) | Err(_) => {}
            Ok(NotificationSetting::DisabledForUser) => return Err("notifications are turned off in Windows Settings".into()),
            Ok(_) => return Err("notifications are turned off for SnipScreen in Windows Settings".into()),
        }
        notifier.Show(&toast).map_err(|e| fail("show the notification", e))
    }
}

/// Escapes text for XML elements and attribute values.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Percent-encodes every byte outside `A-Z a-z 0-9 - . _ ~` and `keep`.
fn pct(s: &str, keep: &[u8]) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) || keep.contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn unpct(s: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(s.len());
    let mut it = s.bytes();
    while let Some(b) = it.next() {
        if b == b'%' {
            let hex = [it.next()?, it.next()?];
            bytes.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8(bytes).ok()
}

fn file_uri(p: &Path) -> String {
    format!("file:///{}", pct(&p.to_string_lossy().replace('\\', "/"), b"/:"))
}

/// The link a toast opens: the snip's id (for the copy kept in memory) and its file (after a restart).
pub fn open_link(id: u64, path: &Path) -> String {
    format!("{SCHEME}open?id={id}&path={}", pct(&path.to_string_lossy(), b""))
}

/// Reads a link made by `open_link`.
pub fn parse_open_link(link: &str) -> Option<(u64, PathBuf)> {
    // `open_link` percent-encodes everything; a raw quote or space means someone else made this.
    if link.chars().any(|c| c == '"' || c.is_whitespace()) {
        return None;
    }
    let rest = link.get(..SCHEME.len()).filter(|s| s.eq_ignore_ascii_case(SCHEME)).map(|_| &link[SCHEME.len()..])?;
    let query = rest.strip_prefix("open?")?;
    let (mut id, mut path) = (None, None);
    for pair in query.split('&') {
        match pair.split_once('=')? {
            ("id", v) => id = v.parse().ok(),
            ("path", v) => path = unpct(v).map(PathBuf::from),
            _ => {}
        }
    }
    Some((id?, path?))
}

/// Links can come from anywhere (any program can open a `snipscreen:` link), so only PNGs directly in
/// the folders SnipScreen writes snips to are opened.
pub fn may_open(path: &Path, folders: &[PathBuf]) -> bool {
    let norm = |p: &Path| p.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase();
    let png = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("png"));
    let plain = path.is_absolute() && path.components().all(|c| !matches!(c, std::path::Component::ParentDir | std::path::Component::CurDir));
    let parent = path.parent().map(norm);
    png && plain && folders.iter().any(|f| parent.as_deref() == Some(norm(f).as_str()))
}

/// The newest snips by id, kept in memory so a click on their notification opens them at once.
/// Bounded by count and by bytes; older ones are opened from their PNG instead.
// ceiling: a byte budget, not a smarter policy (e.g. dropping when the toast leaves the notification
// centre); revisit if people routinely click notifications for big, older snips and find them slow.
pub struct Recent {
    items: VecDeque<(u64, Arc<ColorImage>)>,
    max_items: usize,
    max_bytes: usize,
}

impl Recent {
    pub fn new(max_items: usize, max_bytes: usize) -> Self {
        Self { items: VecDeque::new(), max_items, max_bytes }
    }

    fn bytes(img: &ColorImage) -> usize {
        img.pixels.len() * 4
    }

    pub fn insert(&mut self, id: u64, img: Arc<ColorImage>) {
        self.items.push_back((id, img));
        let mut total: usize = self.items.iter().map(|(_, i)| Self::bytes(i)).sum();
        // The newest one stays even when it alone is over budget: its notification is up right now.
        while self.items.len() > 1 && (self.items.len() > self.max_items || total > self.max_bytes) {
            if let Some((_, old)) = self.items.pop_front() {
                total -= Self::bytes(&old);
            }
        }
    }

    /// Hands the snip over (the editor keeps it from here on).
    pub fn take(&mut self, id: u64) -> Option<Arc<ColorImage>> {
        let at = self.items.iter().position(|(i, _)| *i == id)?;
        self.items.remove(at).map(|(_, img)| img)
    }
}

/// `img` shrunk (box filter) so its longer side is at most `max`; small images are copied as they are.
pub fn preview(img: &ColorImage, max: usize) -> ColorImage {
    let [w, h] = img.size;
    let scale = (max as f32 / w.max(h) as f32).min(1.0);
    if scale >= 1.0 {
        return img.clone();
    }
    let (nw, nh) = (((w as f32 * scale).round() as usize).max(1), ((h as f32 * scale).round() as usize).max(1));
    let mut px = Vec::with_capacity(nw * nh);
    for y in 0..nh {
        let (y0, y1) = (y * h / nh, ((y + 1) * h / nh).max(y * h / nh + 1));
        for x in 0..nw {
            let (x0, x1) = (x * w / nw, ((x + 1) * w / nw).max(x * w / nw + 1));
            let mut sum = [0u32; 4];
            for row in img.pixels[y0 * w..y1 * w].chunks_exact(w) {
                for c in &row[x0..x1] {
                    for (s, v) in sum.iter_mut().zip(c.to_array()) {
                        *s += v as u32;
                    }
                }
            }
            let n = ((x1 - x0) * (y1 - y0)) as u32;
            px.push(Color32::from_rgba_premultiplied((sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8, (sum[3] / n) as u8));
        }
    }
    ColorImage::new([nw, nh], px)
}

pub fn preview_of(img: &ColorImage) -> ColorImage {
    preview(img, PREVIEW_MAX)
}

/// Decodes a PNG file into an image (any 8-bit or 16-bit colour type).
pub fn load_png(path: &Path) -> Result<ColorImage, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut dec = png::Decoder::new(std::io::BufReader::new(file));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().map_err(|e| format!("{}: {e}", path.display()))?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or("the picture is too large")?];
    let info = reader.next_frame(&mut buf).map_err(|e| format!("{}: {e}", path.display()))?;
    let (w, h) = (info.width as usize, info.height as usize);
    let data = &buf[..info.buffer_size()];
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => data.to_vec(),
        png::ColorType::Rgb => data.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => data.as_chunks::<2>().0.iter().flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => data.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return Err(format!("{}: unexpected palette image", path.display())),
    };
    if rgba.len() != w * h * 4 {
        return Err(format!("{}: the picture data is incomplete", path.display()));
    }
    Ok(ColorImage::from_rgba_unmultiplied([w, h], &rgba))
}

/// The snip id in a `recent` file name (`<kind>-<id>.png`).
fn file_id(name: &str) -> Option<u64> {
    name.strip_suffix(".png").and_then(|s| s.rsplit_once('-')).and_then(|(_, id)| id.parse::<u64>().ok())
}

/// The highest snip id with a file in `recent`, 0 when there is none. New ids start above it, so the
/// newest snip always has the highest id (pruning relies on that) even if the clock went back.
pub fn newest_id(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries.filter_map(|e| file_id(e.ok()?.file_name().to_str()?)).max().unwrap_or(0)
}

/// File names in `recent` that belong to snips older than the newest `keep` (named `<kind>-<id>.png`).
fn stale(names: &[String], keep: usize) -> Vec<String> {
    let mut ids: Vec<u64> = names.iter().filter_map(|n| file_id(n)).collect();
    ids.sort_unstable_by(|a, b| b.cmp(a));
    ids.dedup();
    let Some(&cutoff) = ids.get(keep) else { return Vec::new() };
    names.iter().filter(|n| file_id(n).is_some_and(|i| i <= cutoff)).cloned().collect()
}

/// Deletes previews and unsaved snips beyond the newest `RECENT_FILES`. Only names SnipScreen gives
/// its own files are touched.
pub fn prune_recent(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let names: Vec<String> = entries.filter_map(|e| e.ok()?.file_name().into_string().ok()).filter(|n| n.starts_with("preview-") || n.starts_with("snip-")).collect();
    for name in stale(&names, RECENT_FILES) {
        let _ = std::fs::remove_file(dir.join(name));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toast_xml_escapes_text_and_links() {
        let t = Toast {
            title: "Snip copied to clipboard".into(),
            lines: vec!["Saved as <Snip> & \"more\"".into(), "Select to mark it up".into(), "dropped".into()],
            image: Some(PathBuf::from(r"C:\Users\A B\AppData\Local\SnipScreen\recent\preview-7.png")),
            launch: "snipscreen:open?id=7&path=C%3A".into(),
        };
        let xml = t.xml();
        assert!(xml.starts_with("<toast activationType=\"protocol\" launch=\"snipscreen:open?id=7&amp;path=C%3A\">"), "{xml}");
        assert!(xml.contains("<text>Saved as &lt;Snip&gt; &amp; &quot;more&quot;</text><text>Select to mark it up</text><image"), "{xml}");
        assert!(!xml.contains("dropped"), "ToastGeneric has room for a title and two lines");
        assert!(xml.contains("src=\"file:///C:/Users/A%20B/AppData/Local/SnipScreen/recent/preview-7.png\""), "{xml}");
        assert!(XmlDocument::new().unwrap().LoadXml(&HSTRING::from(xml)).is_ok(), "Windows' own parser accepts it");
    }

    #[test]
    fn open_links_round_trip_any_path() {
        let p = PathBuf::from(r"C:\Users\Zoë\Pictures\Screenshots\Snip 2026-11-05 141530 (2).png");
        let link = open_link(42, &p);
        assert!(link.starts_with("snipscreen:open?id=42&path=C%3A%5CUsers%5CZo%C3%AB"), "{link}");
        assert!(!link.contains(' ') && !link.contains('"'), "safe inside a quoted command line");
        assert_eq!(parse_open_link(&link), Some((42, p)));
        assert_eq!(parse_open_link(&link.to_uppercase().replacen("OPEN?ID", "open?id", 1).replacen("&PATH", "&path", 1)).map(|(i, _)| i), Some(42));
        for bad in ["snipscreen:open?id=x&path=C%3A", "snipscreen:open?id=1", "snipscreen:close?id=1&path=a", "http:open?id=1&path=a", "snipscreen:open?id=1&path=%E", "s"] {
            assert_eq!(parse_open_link(bad), None, "{bad}");
        }
    }

    #[test]
    fn links_with_quotes_or_spaces_are_refused() {
        for bad in [
            "snipscreen:open?id=1&path=C%3A%5Cx.png\" --screenshot \"C:\\evil.png",
            "snipscreen:open?id=1&path=C%3A%5Cx.png --tray",
            "snipscreen:open?id=1&path=C%3A%5Cx.png\t",
            "snipscreen:open?id=1\"&path=C%3A%5Cx.png",
        ] {
            assert_eq!(parse_open_link(bad), None, "{bad}");
        }
        assert!(parse_open_link("snipscreen:open?id=1&path=C%3A%5Cx.png").is_some());
    }

    #[test]
    fn the_link_handler_passes_the_link_after_open_link() {
        assert_eq!(open_command(r"C:\Program Files\SnipScreen\snipscreen.exe"), r#""C:\Program Files\SnipScreen\snipscreen.exe" --open-link "%1""#);
    }

    #[test]
    fn only_snips_in_our_folders_may_be_opened() {
        let folders = [PathBuf::from(r"C:\Users\a\Pictures\Screenshots"), PathBuf::from(r"C:\Users\a\AppData\Local\SnipScreen\recent\")];
        assert!(may_open(Path::new(r"C:\Users\a\Pictures\Screenshots\Snip 1.png"), &folders));
        assert!(may_open(Path::new(r"c:\users\A\appdata\local\snipscreen\recent\snip-9.PNG"), &folders));
        assert!(!may_open(Path::new(r"C:\Users\a\Pictures\Screenshots\notes.txt"), &folders));
        assert!(!may_open(Path::new(r"C:\Users\a\Pictures\Screenshots\..\secret.png"), &folders));
        assert!(!may_open(Path::new(r"C:\Users\a\Pictures\Screenshots\sub\x.png"), &folders));
        assert!(!may_open(Path::new(r"\\server\share\x.png"), &folders));
        assert!(!may_open(Path::new("x.png"), &folders));
    }

    fn img(w: usize, h: usize) -> Arc<ColorImage> {
        Arc::new(ColorImage::new([w, h], vec![Color32::RED; w * h]))
    }

    #[test]
    fn recent_snips_are_bounded_by_count_and_bytes() {
        let mut r = Recent::new(3, 1000);
        for id in 1..=4 {
            r.insert(id, img(5, 5)); // 100 bytes each
        }
        assert!(r.take(1).is_none(), "the oldest went past the count");
        assert!(r.take(4).is_some());
        assert!(r.take(4).is_none(), "taken once");
        r.insert(5, img(10, 10)); // 400 bytes
        r.insert(6, img(10, 10));
        r.insert(8, img(15, 15)); // 900 bytes: 5 and 6 must go to stay under 1000
        assert!(r.take(5).is_none() && r.take(6).is_none(), "pushed out by bytes");
        assert!(r.take(8).is_some());
        r.insert(7, img(100, 100)); // 40 000 bytes, alone over budget
        assert!(r.take(6).is_none());
        assert!(r.take(7).is_some(), "the newest stays even over budget");
    }

    #[test]
    fn preview_keeps_the_aspect_and_averages() {
        let mut px = vec![Color32::BLACK; 2000 * 1000];
        for (i, p) in px.iter_mut().enumerate() {
            if i % 2 == 0 {
                *p = Color32::WHITE;
            }
        }
        let p = preview(&ColorImage::new([2000, 1000], px), 728);
        assert_eq!(p.size, [728, 364]);
        let mid = p.pixels[182 * 728 + 364];
        assert!((120..=135).contains(&mid.r()), "black and white average to grey, got {mid:?}");
        assert_eq!(preview(&img(300, 200), 728).size, [300, 200], "small snips are not enlarged");
        assert_eq!(preview(&img(5000, 3), 728).size, [728, 1], "never zero pixels tall");
    }

    #[test]
    fn png_files_load_back() {
        let path = std::env::temp_dir().join(format!("snipscreen-load-{}.png", std::process::id()));
        let src = ColorImage::new([3, 2], vec![Color32::from_rgba_unmultiplied(10, 20, 30, 255), Color32::TRANSPARENT, Color32::WHITE, Color32::RED, Color32::GREEN, Color32::BLUE]);
        let saved = win::save_png(&path, &src);
        let loaded = load_png(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(saved, Ok(()));
        assert_eq!(loaded.map(|i| (i.size, i.pixels)), Ok((src.size, src.pixels)));
        assert!(load_png(Path::new("Z:\\no\\such.png")).is_err());
    }

    #[test]
    fn pruning_keeps_the_newest_snips_and_their_previews() {
        let names: Vec<String> = (1..=25u64).flat_map(|i| [format!("preview-{i}.png"), format!("snip-{i}.png")]).chain(["notes.txt".to_string()]).collect();
        let mut gone = stale(&names, 20);
        gone.sort();
        let mut want: Vec<String> = (1..=5u64).flat_map(|i| [format!("preview-{i}.png"), format!("snip-{i}.png")]).collect();
        want.sort();
        assert_eq!(gone, want);
        assert!(stale(&names[..10], 20).is_empty());
    }

    #[test]
    fn new_ids_start_above_the_newest_file() {
        let dir = std::env::temp_dir().join(format!("snipscreen-ids-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["preview-1762400000000.png", "snip-1762400000123.png", "preview-1762400000123.png", "notes-x.png", "snip-9.txt"] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        let newest = newest_id(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(newest, 1762400000123);
        assert_eq!(newest_id(Path::new("Z:\\no\\such\\dir")), 0);
    }
}
