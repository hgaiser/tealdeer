use std::{
    env,
    ffi::OsStr,
    fs,
    io::Read,
    iter,
    path::{Path, PathBuf},
};

use app_dirs::{get_app_root, AppDataType};
use flate2::read::GzDecoder;
use log::debug;
use reqwest::{blocking::Client, Proxy};
use std::time::{Duration, SystemTime};
use tar::Archive;
use walkdir::{DirEntry, WalkDir};

use crate::{
    error::TealdeerError::{self, CacheError, UpdateError},
    types::{OsType, PathSource},
};

static CACHE_DIR_ENV_VAR: &str = "TEALDEER_CACHE_DIR";

#[derive(Debug)]
pub struct Cache {
    url: String,
    os: OsType,
}

#[derive(Debug)]
pub struct PageLookupResult {
    page_path: PathBuf,
    patch_path: Option<PathBuf>,
}

impl PageLookupResult {
    pub fn with_page(page_path: PathBuf) -> Self {
        Self {
            page_path,
            patch_path: None,
        }
    }

    pub fn with_optional_patch(mut self, patch_path: Option<PathBuf>) -> Self {
        self.patch_path = patch_path;
        self
    }

    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        iter::once(self.page_path.as_path()).chain(self.patch_path.as_deref())
    }
}

impl Cache {
    pub fn new<S>(url: S, os: OsType) -> Self
    where
        S: Into<String>,
    {
        Self {
            url: url.into(),
            os,
        }
    }

    /// Return the path to the cache directory.
    pub fn get_cache_dir() -> Result<(PathBuf, PathSource), TealdeerError> {
        // Allow overriding the cache directory by setting the env variable.
        if let Ok(value) = env::var(CACHE_DIR_ENV_VAR) {
            let path = PathBuf::from(value);
            let (path_exists, path_is_dir) = path
                .metadata()
                .map_or((false, false), |md| (true, md.is_dir()));
            if path_exists && !path_is_dir {
                return Err(CacheError(format!(
                    "Path specified by ${} is not a directory.",
                    CACHE_DIR_ENV_VAR
                )));
            }
            if !path_exists {
                // Try to create the complete directory path.
                fs::create_dir_all(&path).map_err(|_| {
                    CacheError(format!(
                        "Directory path specified by ${} cannot be created.",
                        CACHE_DIR_ENV_VAR
                    ))
                })?;
                eprintln!(
                    "Successfully created cache directory path `{}`.",
                    path.to_str().unwrap()
                );
            }
            return Ok((path, PathSource::EnvVar));
        };

        // Otherwise, fall back to user cache directory.
        match get_app_root(AppDataType::UserCache, &crate::APP_INFO) {
            Ok(dirs) => Ok((dirs, PathSource::OsConvention)),
            Err(_) => Err(CacheError(
                "Could not determine user cache directory.".into(),
            )),
        }
    }

    /// Download the archive
    fn download(&self) -> Result<Vec<u8>, TealdeerError> {
        let mut builder = Client::builder();
        if let Ok(ref host) = env::var("HTTP_PROXY") {
            if let Ok(proxy) = Proxy::http(host) {
                builder = builder.proxy(proxy);
            }
        }
        if let Ok(ref host) = env::var("HTTPS_PROXY") {
            if let Ok(proxy) = Proxy::https(host) {
                builder = builder.proxy(proxy);
            }
        }
        let client = builder.build().unwrap_or_else(|_| Client::new());
        let mut resp = client.get(&self.url).send()?;
        let mut buf: Vec<u8> = vec![];
        let bytes_downloaded = resp.copy_to(&mut buf)?;
        debug!("{} bytes downloaded", bytes_downloaded);
        Ok(buf)
    }

    /// Decompress and open the archive
    fn decompress<R: Read>(reader: R) -> Archive<GzDecoder<R>> {
        Archive::new(GzDecoder::new(reader))
    }

    /// Update the pages cache.
    pub fn update(&self) -> Result<(), TealdeerError> {
        // First, download the compressed data
        let bytes: Vec<u8> = self.download()?;

        // Decompress the response body into an `Archive`
        let mut archive = Self::decompress(&bytes[..]);

        // Determine paths
        let (cache_dir, _) = Self::get_cache_dir()?;

        // Make sure that cache directory exists
        debug!("Ensure cache directory {:?} exists", &cache_dir);
        fs::create_dir_all(&cache_dir)
            .map_err(|e| UpdateError(format!("Could not create cache directory: {}", e)))?;

        // Clear cache directory
        // Note: This is not the best solution. Ideally we would download the
        // archive to a temporary directory and then swap the two directories.
        // But renaming a directory doesn't work across filesystems and Rust
        // does not yet offer a recursive directory copying function. So for
        // now, we'll use this approach.
        Self::clear()?;

        // Extract archive
        archive
            .unpack(&cache_dir)
            .map_err(|e| UpdateError(format!("Could not unpack compressed data: {}", e)))?;

        Ok(())
    }

    /// Return the duration since the cache directory was last modified.
    pub fn last_update() -> Option<Duration> {
        if let Ok((cache_dir, _)) = Self::get_cache_dir() {
            if let Ok(metadata) = fs::metadata(cache_dir.join("tldr-master")) {
                if let Ok(mtime) = metadata.modified() {
                    let now = SystemTime::now();
                    return now.duration_since(mtime).ok();
                };
            };
        };
        None
    }

    /// Return the platform directory.
    fn get_platform_dir(&self) -> Option<&'static str> {
        match self.os {
            OsType::Linux => Some("linux"),
            OsType::OsX => Some("osx"),
            OsType::SunOs => Some("sunos"),
            OsType::Windows => Some("windows"),
        }
    }

    /// Check for pages for a given platform in one of the given languages.
    fn find_page_for_platform(
        page_name: &str,
        cache_dir: &Path,
        platform: &str,
        language_dirs: &[String],
    ) -> Option<PathBuf> {
        language_dirs
            .iter()
            .map(|lang_dir| cache_dir.join(lang_dir).join(platform).join(page_name))
            .find(|path| path.exists() && path.is_file())
    }

    /// Look up custom patch (<name>.patch). If it exists, store it in a variable.
    fn find_patch(patch_name: &str, custom_pages_dir: Option<&Path>) -> Option<PathBuf> {
        custom_pages_dir
            .map(|custom_dir| custom_dir.join(patch_name))
            .filter(|path| path.exists() && path.is_file())
    }

    /// Search for a page and return the path to it.
    pub fn find_page(
        &self,
        name: &str,
        languages: &[String],
        custom_pages_dir: Option<&Path>,
    ) -> Option<PageLookupResult> {
        let page_filename = format!("{}.md", name);
        let patch_filename = format!("{}.patch", name);
        let custom_filename = format!("{}.page", name);

        // Get cache dir
        let cache_dir = match Self::get_cache_dir() {
            Ok((cache_dir, _)) => cache_dir.join("tldr-master"),
            Err(e) => {
                log::error!("Could not get cache directory: {}", e);
                return None;
            }
        };

        let lang_dirs: Vec<String> = languages
            .iter()
            .map(|lang| {
                if lang == "en" {
                    String::from("pages")
                } else {
                    format!("pages.{}", lang)
                }
            })
            .collect();

        // Look up custom page (<name>.page). If it exists, return it directly
        if let Some(config_dir) = custom_pages_dir {
            let custom_page = config_dir.join(custom_filename);
            if custom_page.exists() && custom_page.is_file() {
                return Some(PageLookupResult::with_page(custom_page));
            }
        }

        let patch_path = Self::find_patch(&patch_filename, custom_pages_dir);

        // Try to find a platform specific path next, append custom patch to it.
        if let Some(pf) = self.get_platform_dir() {
            if let Some(page) =
                Self::find_page_for_platform(&page_filename, &cache_dir, pf, &lang_dirs)
            {
                return Some(PageLookupResult::with_page(page).with_optional_patch(patch_path));
            }
        }

        // Did not find platform specific results, fall back to "common"
        Self::find_page_for_platform(&page_filename, &cache_dir, "common", &lang_dirs)
            .map(|page| PageLookupResult::with_page(page).with_optional_patch(patch_path))
    }

    /// Return the available pages.
    pub fn list_pages(&self) -> Result<Vec<String>, TealdeerError> {
        // Determine platforms directory and platform
        let (cache_dir, _) = Self::get_cache_dir()?;
        let platforms_dir = cache_dir.join("tldr-master").join("pages");
        let platform_dir = self.get_platform_dir();

        // Closure that allows the WalkDir instance to traverse platform
        // specific and common page directories, but not others.
        let should_walk = |entry: &DirEntry| -> bool {
            let file_type = entry.file_type();
            let file_name = match entry.file_name().to_str() {
                Some(name) => name,
                None => return false,
            };
            if file_type.is_dir() {
                if file_name == "common" {
                    return true;
                }
                if let Some(platform) = platform_dir {
                    return file_name == platform;
                }
            } else if file_type.is_file() {
                return true;
            }
            false
        };

        // Recursively walk through common and (if applicable) platform specific directory
        let mut pages = WalkDir::new(platforms_dir)
            .min_depth(1) // Skip root directory
            .into_iter()
            .filter_entry(|e| should_walk(e)) // Filter out pages for other architectures
            .filter_map(Result::ok) // Convert results to options, filter out errors
            .filter_map(|e| {
                let path = e.path();
                let extension = &path.extension().and_then(OsStr::to_str).unwrap_or("");
                if e.file_type().is_file() && extension == &"md" {
                    path.file_stem()
                        .and_then(|stem| stem.to_str().map(|s| s.into()))
                } else {
                    None
                }
            })
            .collect::<Vec<String>>();
        pages.sort();
        pages.dedup();
        Ok(pages)
    }

    /// Delete the cache directory.
    pub fn clear() -> Result<(), TealdeerError> {
        let (path, _) = Self::get_cache_dir()?;
        if path.exists() && path.is_dir() {
            fs::remove_dir_all(&path).map_err(|_| {
                CacheError(format!(
                    "Could not remove cache directory ({}).",
                    path.display()
                ))
            })?;
        } else if path.exists() {
            return Err(CacheError(format!(
                "Cache path ({}) is not a directory.",
                path.display()
            )));
        } else {
            return Err(CacheError(format!(
                "Cache path ({}) does not exist.",
                path.display()
            )));
        };
        Ok(())
    }
}

/// Unit Tests for cache module
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_page_lookup_result_iter_with_patch() {
        let lookup = PageLookupResult::with_page(PathBuf::from("test.page"))
            .with_optional_patch(Some(PathBuf::from("test.patch")));
        let mut iter = lookup.paths();
        assert_eq!(iter.next(), Some(Path::new("test.page")));
        assert_eq!(iter.next(), Some(Path::new("test.patch")));
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_page_lookup_result_iter_no_patch() {
        let lookup = PageLookupResult::with_page(PathBuf::from("test.page"));
        let mut iter = lookup.paths();
        assert_eq!(iter.next(), Some(Path::new("test.page")));
        assert_eq!(iter.next(), None);
    }
}
