use crate::disk_cache::{DiskCache, DiskCacheError};
use backon::{ExponentialBuilder, Retryable};
use cache_control::CacheControl;
use reqwest::{
    header::{CACHE_CONTROL, LAST_MODIFIED},
    Client, IntoUrl, Response,
};
use thiserror::Error;

#[derive(Clone)]
pub struct Downloader {
    client: Client,
    url_to_etag_cache: DiskCache<String, String>,
    etag_to_file_cache: DiskCache<String, Vec<u8>>,
}

pub struct DownloadedFile {
    pub contents: Vec<u8>,
    #[allow(dead_code)]
    pub was_cached: bool,
}

#[derive(Error, Debug)]
pub enum DownloadError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Disk cache error: {0}")]
    DiskCache(#[from] DiskCacheError),
    #[error("Header parse error {0}")]
    HeaderParse(#[from] reqwest::header::ToStrError),
}

impl Downloader {
    fn cache_key_from_response(response: &Response) -> Result<String, DownloadError> {
        match response.headers().get("etag") {
            Some(etag) => Ok(etag.to_str()?.to_string()),
            None => match response.headers().get(LAST_MODIFIED) {
                Some(last_modified) => Ok(format!(
                    "url:{},last_modified:{}",
                    response.url().as_str(),
                    last_modified.to_str()?
                )),
                None => Ok(response.url().as_str().to_string()),
            },
        }
    }

    fn update_url_etag_cache(&self, response: &Response) -> Result<String, DownloadError> {
        let etag = Self::cache_key_from_response(response)?;

        if let Some(cache_control) = response.headers().get(CACHE_CONTROL) {
            if let Some(parsed) = CacheControl::from_value(cache_control.to_str().unwrap()) {
                if let Some(max_age) = parsed.max_age {
                    self.url_to_etag_cache.set(
                        &response.url().as_str().to_string(),
                        &etag,
                        max_age.as_secs(),
                    )?;
                }
            }
        }

        Ok(etag)
    }
}

impl Downloader {
    pub fn create() -> Result<Self, DiskCacheError> {
        Ok(Self {
            client: Client::new(),
            url_to_etag_cache: DiskCache::create("url_to_etag")?,
            etag_to_file_cache: DiskCache::create("etag_to_file")?,
        })
    }

    pub async fn download<U: IntoUrl>(&self, url: U) -> Result<DownloadedFile, DownloadError> {
        let url = url.into_url()?;
        let url_str = url.as_str().to_string();

        if let Some(etag) = self.url_to_etag_cache.get(&url_str)? {
            if let Some(file) = self.etag_to_file_cache.get(&etag)? {
                return Ok(DownloadedFile {
                    contents: file,
                    was_cached: true,
                });
            }
        }

        let download = || async {
            let response = self.client.get(url.clone()).send().await?;
            if let Err(err) = response.error_for_status_ref() {
                return Err(err.into());
            }

            let etag = self.update_url_etag_cache(&response)?;
            // File may still exist/be unmodified, but past its cache-control expiry
            if let Some(file) = self.etag_to_file_cache.get(&etag)? {
                return Ok(DownloadedFile {
                    contents: file,
                    was_cached: true,
                });
            }

            let file = response.bytes().await?.to_vec();
            self.etag_to_file_cache.set(&etag, &file, 1000000000000)?; // etag should be immutable

            Ok(DownloadedFile {
                contents: file,
                was_cached: false,
            })
        };

        download.retry(ExponentialBuilder::default()).await
    }
}
