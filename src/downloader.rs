use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use crate::disk_cache::{DiskCache, DiskCacheError};
use cache_control::CacheControl;
use futures::future::BoxFuture;
use futures::{stream::FuturesUnordered, Stream};
use futures::{FutureExt, StreamExt};
use reqwest::Url;
use reqwest::{
    header::{CACHE_CONTROL, LAST_MODIFIED},
    IntoUrl, Response,
};
use thiserror::Error;
use tokio::sync::Semaphore;

#[derive(Debug, Clone, Copy)]
pub struct DownloadManagerStats {
    pub in_flight_requests: u64,
    pub downloaded_bytes: u64,
    pub downloaded_files: u64,
    pub cached_files: u64,
    pub http_errors: u64,
    pub at: std::time::Instant,
}

impl DownloadManagerStats {
    /// Returns download speed in bytes per second.
    pub fn speed(&self, last_stats: DownloadManagerStats) -> f64 {
        let duration = self.at.duration_since(last_stats.at);
        let bytes = self.downloaded_bytes - last_stats.downloaded_bytes;
        (bytes as f64) / duration.as_secs_f64()
    }
}

struct Inner {
    semaphore: Semaphore,
    client: reqwest::Client,
    url_to_etag_cache: DiskCache<String, String>,
    etag_to_file_cache: DiskCache<String, Vec<u8>>,
    downloaded_bytes: AtomicU64,
    downloaded_files: AtomicU64,
    cached_files: AtomicU64,
    http_errors: AtomicU64,
}

pub struct DownloadManager {
    in_flight: FuturesUnordered<BoxFuture<'static, Result<(Url, Vec<u8>), DownloadManagerError>>>,
    inner: Arc<Inner>,
}

#[derive(Debug, Error)]
pub enum DownloadManagerError {
    #[error("Disk cache error {0}")]
    DiskCache(#[from] DiskCacheError),
    #[error("Header parse error {0}")]
    HeaderParse(#[from] reqwest::header::ToStrError),
    #[error("URL parse error {0}")]
    UrlParse(#[source] reqwest::Error),
    #[error("Request error {0}")]
    Request(#[from] reqwest::Error),
}

impl DownloadManager {
    pub fn build(max_concurrent_requests: usize) -> Result<Self, DownloadManagerError> {
        Ok(Self {
            in_flight: FuturesUnordered::new(),
            inner: Arc::new(Inner {
                semaphore: Semaphore::new(max_concurrent_requests),
                client: reqwest::Client::new(),
                url_to_etag_cache: DiskCache::create("url_to_etag")?,
                etag_to_file_cache: DiskCache::create("etag_to_file")?,
                downloaded_bytes: AtomicU64::new(0),
                downloaded_files: AtomicU64::new(0),
                cached_files: AtomicU64::new(0),
                http_errors: AtomicU64::new(0),
            }),
        })
    }

    fn cache_key_from_response(response: &Response) -> Result<String, DownloadManagerError> {
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

    fn update_url_etag_cache(
        inner: &Inner,
        response: &Response,
    ) -> Result<(), DownloadManagerError> {
        let etag = Self::cache_key_from_response(response)?;

        if let Some(cache_control) = response.headers().get(CACHE_CONTROL) {
            if let Some(parsed) = CacheControl::from_value(cache_control.to_str().unwrap()) {
                if let Some(max_age) = parsed.max_age {
                    inner.url_to_etag_cache.set(
                        &response.url().as_str().to_string(),
                        &etag,
                        max_age.as_secs(),
                    )?;
                }
            }
        }

        Ok(())
    }

    async fn download(inner: &Inner, url: Url) -> Result<(Url, Vec<u8>), DownloadManagerError> {
        let _permit = inner.semaphore.acquire().await.unwrap();

        let url_str = url.as_str().to_string();

        if let Some(etag) = inner.url_to_etag_cache.get(&url_str)? {
            if let Some(file) = inner.etag_to_file_cache.get(&etag)? {
                inner
                    .cached_files
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Ok((url, file));
            }
        }

        let mut response = inner.client.get(url.clone()).send().await.unwrap(); // todo: retry on error
        if let Err(err) = response.error_for_status_ref() {
            inner
                .http_errors
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Err(err.into());
        }

        Self::update_url_etag_cache(inner, &response)?;
        let etag = Self::cache_key_from_response(&response)?;
        // File may still exist/be unmodified, but past its cache-control expiry
        if let Some(file) = inner.etag_to_file_cache.get(&etag)? {
            inner
                .cached_files
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Ok((url, file));
        }

        let mut file = Vec::new();
        while let Some(chunk) = response.chunk().await.unwrap() {
            inner
                .downloaded_bytes
                .fetch_add(chunk.len() as u64, std::sync::atomic::Ordering::Relaxed);
            file.extend_from_slice(&chunk);
        }

        inner.etag_to_file_cache.set(&etag, &file, 1000000000000)?; // etag should be immutable

        inner
            .downloaded_files
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        Ok((url, file))
    }

    pub fn queue_download(&self, url: Url) {
        let inner = self.inner.clone();

        self.in_flight
            .push(async move { Self::download(&inner, url).await }.boxed());
    }

    pub fn stats(&self) -> DownloadManagerStats {
        DownloadManagerStats {
            in_flight_requests: self.in_flight.len() as u64,
            downloaded_bytes: self
                .inner
                .downloaded_bytes
                .load(std::sync::atomic::Ordering::Relaxed),
            downloaded_files: self
                .inner
                .downloaded_files
                .load(std::sync::atomic::Ordering::Relaxed),
            cached_files: self
                .inner
                .cached_files
                .load(std::sync::atomic::Ordering::Relaxed),
            http_errors: self
                .inner
                .http_errors
                .load(std::sync::atomic::Ordering::Relaxed),
            at: std::time::Instant::now(),
        }
    }
}

impl Stream for DownloadManager {
    type Item = Result<(Url, Vec<u8>), DownloadManagerError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.get_mut().in_flight.poll_next_unpin(cx)
    }
}
