use crate::{AssetDetails, AssetKey, AssetRef, DebridProvider, ProviderError, ResolvedAsset};
use serde::Deserialize;
use std::time::{SystemTime, UNIX_EPOCH};

const API_BASE: &str = "https://api.real-debrid.com";

const VIDEO_EXTENSIONS: &[&str] = &[
    ".mkv", ".mp4", ".avi", ".mov", ".wmv", ".ts", ".m2ts", ".webm", ".mpg", ".mpeg",
];

pub struct RealDebridProvider {
    api_token: String,
    base_url: String,
    agent: ureq::Agent,
}

impl RealDebridProvider {
    pub fn new(api_token: String) -> Self {
        Self {
            api_token,
            base_url: API_BASE.to_string(),
            agent: ureq::Agent::new_with_defaults(),
        }
    }

    #[cfg(test)]
    pub fn with_base_url(api_token: String, base_url: String) -> Self {
        Self {
            api_token,
            base_url,
            agent: ureq::Agent::new_with_defaults(),
        }
    }

    fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, ProviderError> {
        let url = format!("{}{}", self.base_url, path);
        let mut response = self
            .agent
            .get(&url)
            .header("Authorization", &format!("Bearer {}", self.api_token))
            .call()
            .map_err(map_ureq_error)?;
        response
            .body_mut()
            .read_json::<T>()
            .map_err(|e| ProviderError::Other(format!("JSON parse error: {e}")))
    }

    fn post_form_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        form: &[(&str, &str)],
    ) -> Result<T, ProviderError> {
        let url = format!("{}{}", self.base_url, path);
        let owned: Vec<(String, String)> = form
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut response = self
            .agent
            .post(&url)
            .header("Authorization", &format!("Bearer {}", self.api_token))
            .send_form(owned)
            .map_err(map_ureq_error)?;
        response
            .body_mut()
            .read_json::<T>()
            .map_err(|e| ProviderError::Other(format!("JSON parse error: {e}")))
    }
}

impl DebridProvider for RealDebridProvider {
    fn name(&self) -> &str {
        "realdebrid"
    }

    fn list_candidates(&self) -> Result<Vec<AssetRef>, ProviderError> {
        // List torrents and find video files
        let torrents: Vec<RdTorrent> =
            self.get_json("/rest/1.0/torrents?limit=200")?;

        let mut candidates = Vec::new();

        for torrent in &torrents {
            if torrent.status.as_deref() != Some("downloaded") {
                continue;
            }

            let info: RdTorrentInfo =
                self.get_json(&format!("/rest/1.0/torrents/info/{}", torrent.id))?;

            let selected_files: Vec<&RdTorrentFile> =
                info.files.iter().filter(|f| f.selected == 1).collect();

            let file_link_pairs: Vec<(&RdTorrentFile, &str)> = if !selected_files.is_empty()
                && selected_files.len() == info.links.len()
            {
                selected_files
                    .iter()
                    .zip(info.links.iter())
                    .map(|(f, l)| (*f, l.as_str()))
                    .collect()
            } else {
                info.files
                    .iter()
                    .zip(info.links.iter())
                    .map(|(f, l)| (f, l.as_str()))
                    .collect()
            };

            for (file, _link) in file_link_pairs {
                let size = file.bytes.unwrap_or(0) as u64;
                if size == 0 {
                    continue;
                }
                let filename = extract_filename(file.path.as_deref());
                if !is_playable(filename.as_deref()) {
                    continue;
                }
                let fname = filename.unwrap_or_else(|| "unknown".to_string());
                candidates.push(AssetRef {
                    key: AssetKey {
                        provider: "realdebrid".to_string(),
                        item_id: torrent.id.clone(),
                        file_index: Some(file.id as u32),
                        size_bytes: size,
                    },
                    filename: fname,
                });
            }
        }

        // Sort by size descending (prefer larger files for benchmarking)
        candidates.sort_by(|a, b| b.key.size_bytes.cmp(&a.key.size_bytes));
        Ok(candidates)
    }

    fn inspect(&self, asset: &AssetRef) -> Result<AssetDetails, ProviderError> {
        Ok(AssetDetails {
            key: asset.key.clone(),
            filename: asset.filename.clone(),
            mime_type: None,
        })
    }

    fn resolve_direct_url(&self, asset: &AssetRef) -> Result<ResolvedAsset, ProviderError> {
        // Get torrent info to find the link for this file
        let info: RdTorrentInfo =
            self.get_json(&format!("/rest/1.0/torrents/info/{}", asset.key.item_id))?;

        let selected_files: Vec<&RdTorrentFile> =
            info.files.iter().filter(|f| f.selected == 1).collect();

        let file_link_pairs: Vec<(&RdTorrentFile, &str)> = if !selected_files.is_empty()
            && selected_files.len() == info.links.len()
        {
            selected_files
                .iter()
                .zip(info.links.iter())
                .map(|(f, l)| (*f, l.as_str()))
                .collect()
        } else {
            info.files
                .iter()
                .zip(info.links.iter())
                .map(|(f, l)| (f, l.as_str()))
                .collect()
        };

        let file_index = asset.key.file_index.unwrap_or(0);
        let (_file, link) = file_link_pairs
            .iter()
            .find(|(f, _)| f.id as u32 == file_index)
            .ok_or_else(|| {
                ProviderError::NotFound(format!(
                    "File index {} not found in torrent {}",
                    file_index, asset.key.item_id
                ))
            })?;

        // Unrestrict the link to get a direct URL
        let unrestricted: RdUnrestricted =
            self.post_form_json("/rest/1.0/unrestrict/link", &[("link", link)])?;

        let direct_url = unrestricted
            .download
            .ok_or_else(|| ProviderError::Other("Unrestrict response missing download URL".into()))?;

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        Ok(ResolvedAsset {
            key: asset.key.clone(),
            direct_url,
            filename: unrestricted.filename.unwrap_or_else(|| asset.filename.clone()),
            size_bytes: unrestricted.filesize.unwrap_or(asset.key.size_bytes),
            resolved_at_ms: now_ms,
            expiry_hint: None,
            provider_meta: None,
        })
    }
}

// --- Real-Debrid API response types ---

#[derive(Debug, Deserialize)]
struct RdTorrent {
    id: String,
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RdTorrentInfo {
    #[allow(dead_code)]
    id: String,
    files: Vec<RdTorrentFile>,
    #[serde(default)]
    links: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RdTorrentFile {
    id: i64,
    path: Option<String>,
    bytes: Option<i64>,
    #[serde(default)]
    selected: i32,
}

#[derive(Debug, Deserialize)]
struct RdUnrestricted {
    filename: Option<String>,
    filesize: Option<u64>,
    download: Option<String>,
}

// --- Helpers ---

fn extract_filename(path: Option<&str>) -> Option<String> {
    let p = path?.trim().trim_end_matches('/');
    if p.is_empty() {
        return None;
    }
    Some(p.rsplit('/').next().unwrap_or(p).to_string())
}

fn is_playable(filename: Option<&str>) -> bool {
    let name = filename.unwrap_or("").to_lowercase();
    VIDEO_EXTENSIONS.iter().any(|ext| name.ends_with(ext))
}

fn map_ureq_error(e: ureq::Error) -> ProviderError {
    match &e {
        ureq::Error::StatusCode(code) => {
            if *code == 401 || *code == 403 {
                ProviderError::AuthFailed(format!("HTTP {code}"))
            } else if *code == 429 {
                ProviderError::RateLimited {
                    retry_after_secs: 60,
                }
            } else {
                ProviderError::Network(format!("HTTP {code}: {e}"))
            }
        }
        _ => ProviderError::Network(e.to_string()),
    }
}
