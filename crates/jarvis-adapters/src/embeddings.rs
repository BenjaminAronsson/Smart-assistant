//! Lazy CPU embeddings (M4, docs/03 §3, docs/09 §5).
//!
//! The model is loaded only on the first request and is serialized behind one
//! worker operation. FastEmbed's ONNX call is synchronous; it runs in a
//! tracked `spawn_blocking` task and the cancellation token is checked both
//! before and after the bounded inference. The task is awaited even if the
//! caller cancels, so no blocking worker is detached.

use async_trait::async_trait;
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use jarvis_application::ports::{EmbeddingError, EmbeddingProvider};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const DIMENSIONS: usize = 384;
const MAX_TEXT_BYTES: usize = 2_000;

#[derive(Debug, Clone)]
pub struct FastEmbedConfig {
    pub model: String,
    pub cache_dir: PathBuf,
    pub intra_threads: usize,
    pub idle_unload_secs: u64,
}

impl Default for FastEmbedConfig {
    fn default() -> Self {
        Self {
            model: "bge-small-en-v1.5".to_owned(),
            cache_dir: PathBuf::from("/var/lib/jarvis/models"),
            intra_threads: 2,
            idle_unload_secs: 600,
        }
    }
}

struct LoadedModel {
    model: TextEmbedding,
    last_used: Instant,
}

pub struct FastEmbedProvider {
    config: FastEmbedConfig,
    loaded: Arc<Mutex<Option<LoadedModel>>>,
    idle_timer: Mutex<Option<JoinHandle<()>>>,
}

impl FastEmbedProvider {
    pub fn new(config: FastEmbedConfig) -> Self {
        Self {
            config,
            loaded: Arc::new(Mutex::new(None)),
            idle_timer: Mutex::new(None),
        }
    }

    fn arm_idle_unload(&self) {
        let Ok(mut timer) = self.idle_timer.lock() else {
            return;
        };
        if let Some(previous) = timer.take() {
            previous.abort();
        }
        let loaded = Arc::clone(&self.loaded);
        let delay = Duration::from_secs(self.config.idle_unload_secs);
        *timer = Some(tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            if let Ok(mut guard) = loaded.lock()
                && guard
                    .as_ref()
                    .is_some_and(|model| model.last_used.elapsed() >= delay)
            {
                *guard = None;
            }
        }));
    }

    fn embed_sync(
        config: &FastEmbedConfig,
        loaded: &Mutex<Option<LoadedModel>>,
        text: String,
    ) -> Result<Vec<f32>, EmbeddingError> {
        let mut guard = loaded.lock().map_err(|_| EmbeddingError::Failed)?;
        if let Some(existing) = guard.as_ref()
            && existing.last_used.elapsed().as_secs() >= config.idle_unload_secs
        {
            *guard = None;
        }
        if guard.is_none() {
            let options = TextInitOptions::new(EmbeddingModel::BGESmallENV15)
                .with_cache_dir(config.cache_dir.clone())
                .with_show_download_progress(false)
                .with_intra_threads(config.intra_threads);
            let model = TextEmbedding::try_new(options).map_err(|_| EmbeddingError::Unavailable)?;
            *guard = Some(LoadedModel {
                model,
                last_used: Instant::now(),
            });
        }
        let loaded = guard.as_mut().ok_or(EmbeddingError::Unavailable)?;
        let vectors = loaded
            .model
            .embed(vec![text], Some(1))
            .map_err(|_| EmbeddingError::Failed)?;
        loaded.last_used = Instant::now();
        let vector = vectors.into_iter().next().ok_or(EmbeddingError::Failed)?;
        if vector.len() != DIMENSIONS || vector.iter().any(|value| !value.is_finite()) {
            return Err(EmbeddingError::InvalidDimensions);
        }
        Ok(vector)
    }
}

#[async_trait]
impl EmbeddingProvider for FastEmbedProvider {
    fn model_id(&self) -> &str {
        &self.config.model
    }

    fn dimensions(&self) -> usize {
        DIMENSIONS
    }

    async fn embed(
        &self,
        text: &str,
        cancel: &CancellationToken,
    ) -> Result<Vec<f32>, EmbeddingError> {
        if cancel.is_cancelled() {
            return Err(EmbeddingError::Cancelled);
        }
        if text.is_empty() || text.len() > MAX_TEXT_BYTES {
            return Err(EmbeddingError::Failed);
        }
        if self.config.model != "bge-small-en-v1.5" {
            return Err(EmbeddingError::Unavailable);
        }
        let config = self.config.clone();
        let loaded = Arc::clone(&self.loaded);
        let text = text.to_owned();
        let result = tokio::task::spawn_blocking(move || Self::embed_sync(&config, &loaded, text))
            .await
            .map_err(|_| EmbeddingError::Failed)??;
        if cancel.is_cancelled() {
            return Err(EmbeddingError::Cancelled);
        }
        self.arm_idle_unload();
        Ok(result)
    }
}

impl Drop for FastEmbedProvider {
    fn drop(&mut self) {
        if let Ok(mut timer) = self.idle_timer.lock()
            && let Some(handle) = timer.take()
        {
            handle.abort();
        }
    }
}
