use crate::{AssetDetails, AssetKey, AssetRef, DebridProvider, ProviderError, ResolvedAsset};
use serde::Deserialize;
use std::time::{SystemTime, UNIX_EPOCH};

const API_BASE: &str = "https://www.premiumize.me/api";

const VIDEO_EXTENSIONS: &[&str] = &[
    ".mkv", ".mp4", ".avi", ".mov", ".wmv", ".ts", ".m2ts", ".webm", ".mpg", ".mpeg",
];

pub struct PremiumizeProvider {
    api_key: String,
    base_url: String,
    agent: ureq::Agent,
}

impl PremiumizeProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            base_url: API_BASE.to_string(),
            agent: ureq::Agent::new_with_defaults(),
        }
    }

    #[cfg(test)]
    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self {
            api_key,
            base_url,
            agent: ureq::Agent::new_with_defaults(),
        }
    }

    fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        extra_params: &[(&str, &str)],
    ) -> Result<T, ProviderError> {
        let url = format!("{}{}", self.base_url, path);
        let mut req = self.agent.get(&url).query("apikey", &self.api_key);
        for (k, v) in extra_params {
            req = req.query(k, v);
        }
        let mut response = req.call().map_err(map_ureq_error)?;
        response
            .body_mut()
            .read_json::<T>()
            .map_err(|e| ProviderError::Other(format!("JSON parse error: {e}")))
    }
}

impl DebridProvider for PremiumizeProvider {
    fn name(&self) -> &str {
        "premiumize"
    }

    fn list_candidates(&self) -> Result<Vec<AssetRef>, ProviderError> {
        let response: PmListAllResponse = self.get_json("/item/listall", &[])?;

        if response.status.as_deref() != Some("success") {
            return Err(ProviderError::Other(format!(
                "Premiumize API reported status: {:?}",
                response.status
            )));
        }

        let mut candidates = Vec::new();
        for item in response.files.unwrap_or_default() {
            let size = item.size.unwrap_or(0) as u64;
            if size == 0 {
                continue;
            }
            let filename = best_filename(item.name.as_deref(), item.path.as_deref());
            if !is_playable(filename.as_deref(), item.mime_type.as_deref()) {
                continue;
            }
            let fname = filename.unwrap_or_else(|| "unknown".to_string());
            candidates.push(AssetRef {
                key: AssetKey {
                    provider: "premiumize".to_string(),
                    item_id: item.id.clone(),
                    file_index: None,
                    size_bytes: size,
                },
                filename: fname,
            });
        }

        // Sort by size descending (prefer larger files for benchmarking)
        candidates.sort_by(|a, b| b.key.size_bytes.cmp(&a.key.size_bytes));
        Ok(candidates)
    }

    fn inspect(&self, asset: &AssetRef) -> Result<AssetDetails, ProviderError> {
        let details: PmItemDetails =
            self.get_json("/item/details", &[("id", &asset.key.item_id)])?;

        Ok(AssetDetails {
            key: asset.key.clone(),
            filename: details.name.unwrap_or_else(|| asset.filename.clone()),
            mime_type: details.mime_type,
        })
    }

    fn resolve_direct_url(&self, asset: &AssetRef) -> Result<ResolvedAsset, ProviderError> {
        let details: PmItemDetails =
            self.get_json("/item/details", &[("id", &asset.key.item_id)])?;

        // Prefer stream_link over link (matches Python resolver)
        let direct_url = details
            .stream_link
            .or(details.link)
            .ok_or_else(|| {
                ProviderError::NotFound(format!(
                    "Premiumize item {} has no downloadable URL",
                    asset.key.item_id
                ))
            })?;

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        Ok(ResolvedAsset {
            key: asset.key.clone(),
            direct_url,
            filename: details.name.unwrap_or_else(|| asset.filename.clone()),
            size_bytes: details.size.map(|s| s as u64).unwrap_or(asset.key.size_bytes),
            resolved_at_ms: now_ms,
            expiry_hint: None,
            provider_meta: None,
        })
    }
}

// --- Premiumize API response types ---

#[derive(Debug, Deserialize)]
struct PmListAllResponse {
    status: Option<String>,
    files: Option<Vec<PmItem>>,
}

#[derive(Debug, Deserialize)]
struct PmItem {
    id: String,
    name: Option<String>,
    size: Option<i64>,
    mime_type: Option<String>,
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PmItemDetails {
    #[allow(dead_code)]
    id: Option<String>,
    name: Option<String>,
    size: Option<i64>,
    mime_type: Option<String>,
    link: Option<String>,
    stream_link: Option<String>,
}

// --- Helpers ---

fn best_filename(name: Option<&str>, path: Option<&str>) -> Option<String> {
    if let Some(n) = name {
        let trimmed = n.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    extract_filename(path)
}

fn extract_filename(path: Option<&str>) -> Option<String> {
    let p = path?.trim().trim_end_matches('/');
    if p.is_empty() {
        return None;
    }
    Some(p.rsplit('/').next().unwrap_or(p).to_string())
}

fn is_playable(filename: Option<&str>, mime_type: Option<&str>) -> bool {
    if let Some(mime) = mime_type {
        if mime.trim().to_lowercase().starts_with("video/") {
            return true;
        }
    }
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
