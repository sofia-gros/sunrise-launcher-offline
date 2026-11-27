//! Project icons and gallery images, fetched at run time from the sources' image servers and
//! kept on disk: nothing of a project's art is in the launcher. A URL is fetched only over HTTPS
//! from Modrinth's CDN or Sunrise's registry, at most [`MAX_DOWNLOAD`] bytes, decoded (PNG, JPEG, WebP,
//! GIF), scaled down and stored as a PNG the UI draws; the cache is kept under [`MAX_CACHE`] by
//! dropping the oldest files. After [`FRESH`] an image is asked for again with its server's
//! validators (`If-None-Match`, `If-Modified-Since`): unchanged, it costs one small answer.
use sha1::{Digest, Sha1};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::i18n::{tr, trf};

/// Hosts images may come from.
const HOSTS: [&str; 4] = [
    "https://cdn.modrinth.com/",
    sunrise_launcher_addresses::MOD_IMAGES,
    // Horizon's registry, and the mirror kept at the address it had before it moved to horizon-rust.
    "https://horizon-rust.github.io/horizon-mods/",
    "https://ludovic111.github.io/sunrise-mods/",
];
/// The most an image may weigh before decoding.
pub const MAX_DOWNLOAD: u64 = 8 << 20;
/// The most a registry v2 image (icon or screenshot) may weigh before decoding.
pub const MAX_V2_IMAGE: u64 = 5 << 20;
/// The most the cache keeps on disk.
pub const MAX_CACHE: u64 = 128 << 20;
/// Pixels: an icon's side, a gallery image's longest side.
pub const ICON: u32 = 128;
pub const GALLERY: u32 = 960;
/// A list row's icon: 40 points, drawn at twice that on a Retina screen.
pub const ROW: u32 = 80;
/// How long a cached image is used without asking its server whether it changed.
pub const FRESH: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

fn dir(home: &Path) -> PathBuf {
    home.join("mod-browser/images")
}

/// Why `url` is never fetched.
pub fn refuse(url: &str) -> Option<&'static str> {
    if !HOSTS.iter().any(|h| url.starts_with(h)) {
        return Some(tr(
            "not on Modrinth's or Sunrise's image servers over HTTPS",
        ));
    }
    if url.len() > 2048 || url.contains(char::is_whitespace) {
        return Some(tr("not a plain URL"));
    }
    None
}

/// Where the image of `url` scaled to `size` is kept.
pub fn path(home: &Path, url: &str, size: u32) -> PathBuf {
    let hash: String = Sha1::digest(url.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    dir(home).join(format!("{hash}-{size}.png"))
}

/// The image if it is cached already.
pub fn cached(home: &Path, url: &str, size: u32) -> Option<PathBuf> {
    let p = path(home, url, size);
    p.is_file().then_some(p)
}

/// Whether the cached image of `url` is older than [`FRESH`]: [`fetch`] asks its server again.
pub fn stale(home: &Path, url: &str, size: u32) -> bool {
    fs::metadata(path(home, url, size))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age > FRESH)
}

/// The server's validators of a cached image (its `ETag`, then its `Last-Modified`), beside it.
fn validators(image: &Path) -> PathBuf {
    image.with_extension("tag")
}

/// The image of `url`, no larger than `size` pixels a side: the cached one while fresh, else
/// fetched (with the cached one's validators: an unchanged one is kept), decoded, scaled and
/// stored.
pub fn fetch(home: &Path, url: &str, size: u32) -> Result<PathBuf, String> {
    let target = path(home, url, size);
    let cached = target.is_file();
    if cached && !stale(home, url, size) {
        return Ok(target);
    }
    if let Some(why) = refuse(url) {
        return Err(format!("{url}: {why}"));
    }
    let mut request = super::download::agent(url)?.get(url);
    if cached {
        let tags = fs::read_to_string(validators(&target)).unwrap_or_default();
        let mut tags = tags.lines();
        if let Some(etag) = tags.next().filter(|t| !t.is_empty()) {
            request = request.set("If-None-Match", etag);
        }
        if let Some(modified) = tags.next().filter(|t| !t.is_empty()) {
            request = request.set("If-Modified-Since", modified);
        }
    }
    let response = request.call().map_err(|e| format!("{url}: {e}"))?;
    if cached && response.status() == 304 {
        // Unchanged: fresh for another while.
        let _ = fs::File::options()
            .write(true)
            .open(&target)
            .and_then(|f| f.set_modified(std::time::SystemTime::now()));
        return Ok(target);
    }
    let tags = format!(
        "{}\n{}\n",
        response.header("ETag").unwrap_or_default(),
        response.header("Last-Modified").unwrap_or_default()
    );
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_DOWNLOAD + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{url}: {e}"))?;
    if bytes.len() as u64 > MAX_DOWNLOAD {
        return Err(trf(
            "{url}: larger than {size} MB",
            &[("url", url), ("size", &(MAX_DOWNLOAD >> 20).to_string())],
        ));
    }
    let stored = store(home, url, size, &bytes)?;
    let _ = fs::write(validators(&stored), tags);
    Ok(stored)
}

/// Decodes `bytes`, scales them to fit `size` and writes the PNG.
pub fn store(home: &Path, url: &str, size: u32, bytes: &[u8]) -> Result<PathBuf, String> {
    let scaled = decode_and_scale(url, size, bytes)?;
    let target = path(home, url, size);
    let folder = dir(home);
    fs::create_dir_all(&folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    let part = target.with_extension("part");
    scaled
        .save_with_format(&part, image::ImageFormat::Png)
        .map_err(|e| format!("{}: {e}", part.display()))?;
    fs::rename(&part, &target).map_err(|e| format!("{}: {e}", target.display()))?;
    account_write(&folder, file_len(&target), MAX_CACHE);
    Ok(target)
}

fn decode_and_scale(label: &str, size: u32, bytes: &[u8]) -> Result<image::DynamicImage, String> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(256 << 20);
    reader.limits(limits);
    let decoded = reader.decode().map_err(|e| format!("{label}: {e}"))?;
    Ok(if decoded.width() > size || decoded.height() > size {
        decoded.resize(size, size, image::imageops::FilterType::Triangle)
    } else {
        decoded
    })
}

fn v2_dir(home: &Path) -> PathBuf {
    home.join("mod-browser/registry-v2/images")
}

/// Where a verified content-addressed v2 image (`pages/<project>/<sha256>.<png|jpg>`) scaled to
/// `size` is cached on disk. Rejects any path that is not content-addressed for `project_id`.
pub fn v2_path(
    home: &Path,
    project_id: &str,
    relative_path: &str,
    size: u32,
) -> Result<PathBuf, String> {
    let sha256 =
        sunrise_launcher_smod::index_v2::content_addressed_image_sha256(project_id, relative_path)?;
    Ok(v2_dir(home).join(format!("{sha256}-{size}.png")))
}

/// The verified v2 image if it is already cached on disk.
pub fn cached_v2(home: &Path, project_id: &str, relative_path: &str, size: u32) -> Option<PathBuf> {
    let p = v2_path(home, project_id, relative_path, size).ok()?;
    p.is_file().then_some(p)
}

/// Verifies `bytes` against the SHA-256 embedded in `relative_path`, then decodes, scales, and
/// stores the PNG in the content-addressed v2 cache.
pub fn store_v2(
    home: &Path,
    project_id: &str,
    relative_path: &str,
    size: u32,
    bytes: &[u8],
) -> Result<PathBuf, String> {
    if bytes.len() as u64 > MAX_V2_IMAGE {
        return Err(trf(
            "{path}: larger than {size} MB",
            &[
                ("path", relative_path),
                ("size", &(MAX_V2_IMAGE >> 20).to_string()),
            ],
        ));
    }
    sunrise_launcher_smod::index_v2::verify_image_bytes(project_id, relative_path, bytes)?;
    let target = v2_path(home, project_id, relative_path, size)?;
    let scaled = decode_and_scale(relative_path, size, bytes)?;
    let folder = v2_dir(home);
    fs::create_dir_all(&folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    static PART_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = PART_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let part = target.with_extension(format!("part-{seq}"));
    if let Err(e) = scaled.save_with_format(&part, image::ImageFormat::Png) {
        let _ = fs::remove_file(&part);
        return Err(format!("{}: {e}", part.display()));
    }
    if let Err(e) = fs::rename(&part, &target) {
        let _ = fs::remove_file(&part);
        return Err(format!("{}: {e}", target.display()));
    }
    account_write(&folder, file_len(&target), MAX_CACHE);
    Ok(target)
}

/// Loads a content-addressed v2 image relative to `index_location`, verifies its SHA-256 against
/// its file name before decoding, and caches the scaled PNG.
pub fn fetch_v2_from(
    home: &Path,
    index_location: &str,
    project_id: &str,
    relative_path: &str,
    size: u32,
) -> Result<PathBuf, String> {
    let target = v2_path(home, project_id, relative_path, size)?;
    if target.is_file() {
        return Ok(target);
    }
    let bytes = read_v2_bytes(index_location, relative_path)?;
    store_v2(home, project_id, relative_path, size, &bytes)
}

/// Loads a content-addressed v2 image relative to the active v2 registry index location.
pub fn fetch_v2(
    home: &Path,
    project_id: &str,
    relative_path: &str,
    size: u32,
) -> Result<PathBuf, String> {
    fetch_v2_from(
        home,
        &super::sunrise::v2::location(),
        project_id,
        relative_path,
        size,
    )
}

fn read_v2_bytes(index_location: &str, relative_path: &str) -> Result<Vec<u8>, String> {
    let is_http = index_location.starts_with("https://")
        || (cfg!(debug_assertions)
            && (index_location.starts_with("http://127.0.0.1:")
                || index_location.starts_with("http://localhost:")));
    if is_http {
        let url = sunrise_launcher_smod::index_v2::image_url(index_location, relative_path)
            .ok_or_else(|| format!("cannot resolve v2 image path {relative_path}"))?;
        let response = super::download::agent(&url)?
            .get(&url)
            .call()
            .map_err(|e| format!("{url}: {e}"))?;
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take(MAX_V2_IMAGE + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("{url}: {e}"))?;
        if bytes.len() as u64 > MAX_V2_IMAGE {
            return Err(trf(
                "{url}: larger than {size} MB",
                &[("url", &url), ("size", &(MAX_V2_IMAGE >> 20).to_string())],
            ));
        }
        return Ok(bytes);
    }
    if index_location.contains("://") {
        return Err(trf(
            "{index_location}: v2 images require an https:// index or local file path",
            &[("index_location", index_location)],
        ));
    }
    let base = Path::new(index_location).parent().ok_or_else(|| {
        trf(
            "{index_location}: index path has no parent directory",
            &[("index_location", index_location)],
        )
    })?;
    let file = base.join(relative_path);
    let mut bytes = Vec::new();
    fs::File::open(&file)
        .map_err(|e| format!("{}: {e}", file.display()))?
        .take(MAX_V2_IMAGE + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{}: {e}", file.display()))?;
    if bytes.len() as u64 > MAX_V2_IMAGE {
        return Err(trf(
            "{file}: larger than {size} MB",
            &[
                ("file", &file.display().to_string()),
                ("size", &(MAX_V2_IMAGE >> 20).to_string()),
            ],
        ));
    }
    Ok(bytes)
}

/// What the process knows about one cache folder: the bytes it holds, and whether they have been
/// counted since the process started.
#[derive(Default)]
struct CachedFolder {
    bytes: u64,
    counted: bool,
    #[cfg(test)]
    scans: u32,
}

/// The cache's running totals, by folder. Four image workers write at once, so this mutex is also
/// what keeps each write's accounting and its prune a single writer.
static CACHE: std::sync::Mutex<Option<std::collections::HashMap<PathBuf, CachedFolder>>> =
    std::sync::Mutex::new(None);

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn folder_bytes(folder: &Path) -> u64 {
    fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// Accounts for `bytes` just added to `folder`, and prunes once the cap is crossed. The folder is
/// counted by one scan the first time this process writes to it, so a cache left by an earlier run
/// still has its size known; later writes only add to the total.
fn account_write(folder: &Path, bytes: u64, max: u64) {
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let folders = cache.get_or_insert_with(std::collections::HashMap::new);
    let state = folders.entry(folder.to_path_buf()).or_default();
    if state.counted {
        state.bytes = state.bytes.saturating_add(bytes);
    } else {
        state.bytes = folder_bytes(folder);
        state.counted = true;
        #[cfg(test)]
        {
            state.scans += 1;
        }
    }
    if state.bytes > max {
        state.bytes = prune(folder, max);
    }
}

#[cfg(test)]
fn scan_count(folder: &Path) -> u32 {
    let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache
        .as_ref()
        .and_then(|folders| folders.get(folder))
        .map_or(0, |state| state.scans)
}

/// Drops the oldest files until the folder holds at most `max` bytes, and returns what is left.
fn prune(folder: &Path, max: u64) -> u64 {
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some((m.modified().ok()?, m.len(), e.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    files.sort();
    for (_, len, path) in files {
        if total <= max {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
    total
}

/// `bytes` decoded (at most `max_side` pixels a side and `max_alloc` bytes), scaled down to fit
/// `side` pixels, as PNG bytes. None when they aren't an image within those limits.
pub fn fit_png(bytes: Vec<u8>, side: u32, max_side: u32, max_alloc: u64) -> Option<Vec<u8>> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(max_side);
    limits.max_image_height = Some(max_side);
    limits.max_alloc = Some(max_alloc);
    reader.limits(limits);
    let decoded = reader.decode().ok()?;
    let scaled = if decoded.width() > side || decoded.height() > side {
        decoded.resize(side, side, image::imageops::FilterType::Triangle)
    } else {
        decoded
    };
    let mut png = Vec::new();
    scaled
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(png)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_image_goes_stale_after_a_while() {
        let home = std::env::temp_dir().join(format!("stale-{}", std::process::id()));
        let url = "https://cdn.modrinth.com/data/x/icon.png";
        let img = path(&home, url, ROW);
        fs::create_dir_all(img.parent().unwrap()).unwrap();
        fs::write(&img, b"png").unwrap();
        assert!(!stale(&home, url, ROW));
        let old = std::time::SystemTime::now() - FRESH - std::time::Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(&img)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert!(stale(&home, url, ROW));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn only_the_sources_image_servers() {
        assert!(refuse("https://cdn.modrinth.com/data/AANobbMI/icon.png").is_none());
        assert!(refuse("https://media.forgecdn.net/avatars/1/2.png").is_some());
        assert!(refuse("http://cdn.modrinth.com/data/x/icon.png").is_some());
        assert!(refuse("https://cdn.modrinth.com.evil.example/x.png").is_some());
        assert!(refuse("https://example.com/x.png").is_some());
    }

    #[test]
    fn images_are_scaled_stored_and_pruned() {
        let home = std::env::temp_dir().join(format!("sunrise-images-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let big = image::RgbaImage::from_pixel(300, 150, image::Rgba([1, 2, 3, 255]));
        let mut png = Vec::new();
        big.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let url = "https://cdn.modrinth.com/data/x/icon.png";
        let p = store(&home, url, ICON, &png).unwrap();
        assert_eq!(cached(&home, url, ICON), Some(p.clone()));
        assert_eq!(image::open(&p).unwrap().width(), ICON);
        assert!(store(&home, url, ICON, b"not an image").is_err());
        prune(&dir(&home), 0);
        assert!(cached(&home, url, ICON).is_none());
        fs::remove_dir_all(&home).unwrap();
    }

    fn aged_file(folder: &Path, name: &str, len: usize, age: u64) {
        let path = folder.join(name);
        fs::write(&path, vec![0u8; len]).unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(age))
            .unwrap();
    }

    #[test]
    fn a_burst_is_capped_and_keeps_the_newest_files() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("images");
        fs::create_dir_all(&folder).unwrap();
        aged_file(&folder, "old.png", 60, 30);
        account_write(&folder, 60, 120);
        aged_file(&folder, "middle.png", 60, 20);
        account_write(&folder, 60, 120);
        aged_file(&folder, "new.png", 60, 10);
        account_write(&folder, 60, 120);
        assert!(folder_bytes(&folder) <= 120);
        assert!(!folder.join("old.png").exists());
        assert!(folder.join("middle.png").exists());
        assert!(folder.join("new.png").exists());
    }

    #[test]
    fn writes_under_the_cap_count_the_folder_once() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("images");
        fs::create_dir_all(&folder).unwrap();
        aged_file(&folder, "a.png", 60, 10);
        account_write(&folder, 60, 1 << 20);
        assert_eq!(scan_count(&folder), 1);
        aged_file(&folder, "b.png", 60, 5);
        account_write(&folder, 60, 1 << 20);
        assert_eq!(scan_count(&folder), 1);
    }

    #[test]
    fn v2_images_require_content_addressed_path_and_matching_sha256() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let site = dir.path().join("site");
        let index_path = site.join("index.json");
        fs::create_dir_all(&site).unwrap();
        fs::write(&index_path, b"{}").unwrap();

        let mut png = Vec::new();
        image::RgbaImage::from_pixel(200, 100, image::Rgba([40, 120, 200, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let sha256: String = <sha2::Sha256 as sha2::Digest>::digest(&png)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let good_rel = format!("pages/Ab3dE9xQ/{sha256}.png");
        let on_disk = site.join(&good_rel);
        fs::create_dir_all(on_disk.parent().unwrap()).unwrap();
        fs::write(&on_disk, &png).unwrap();

        // Valid content-addressed image verifies, scales, and caches.
        let index_loc = index_path.to_str().unwrap();
        let stored = fetch_v2_from(&home, index_loc, "Ab3dE9xQ", &good_rel, ICON).unwrap();
        assert_eq!(
            cached_v2(&home, "Ab3dE9xQ", &good_rel, ICON),
            Some(stored.clone())
        );
        assert_eq!(image::open(&stored).unwrap().width(), ICON);

        // Tampered bytes whose SHA-256 doesn't match the filename are rejected and never cached.
        let other_sha = "0".repeat(64);
        let tampered_rel = format!("pages/Ab3dE9xQ/{other_sha}.png");
        let tampered_disk = site.join(&tampered_rel);
        fs::write(&tampered_disk, &png).unwrap();
        assert!(fetch_v2_from(&home, index_loc, "Ab3dE9xQ", &tampered_rel, ICON).is_err());
        assert!(cached_v2(&home, "Ab3dE9xQ", &tampered_rel, ICON).is_none());

        // Non-content-addressed paths, external URLs, wrong project IDs, and traversals are rejected.
        assert!(fetch_v2_from(
            &home,
            index_loc,
            "Ab3dE9xQ",
            "pages/Ab3dE9xQ/icon.png",
            ICON
        )
        .is_err());
        assert!(fetch_v2_from(&home, index_loc, "OtherId1", &good_rel, ICON).is_err());
        assert!(fetch_v2_from(
            &home,
            index_loc,
            "Ab3dE9xQ",
            "https://example.com/icon.png",
            ICON,
        )
        .is_err());
        assert!(fetch_v2_from(
            &home,
            index_loc,
            "Ab3dE9xQ",
            &format!("pages/Ab3dE9xQ/../Ab3dE9xQ/{sha256}.png"),
            ICON,
        )
        .is_err());
    }
}
