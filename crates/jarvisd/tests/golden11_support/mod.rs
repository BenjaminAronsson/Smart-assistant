//! Support doubles for golden 11 — deliberately only the two things M7 is not
//! proving: which artifact exists, and where placement audit rows land.
//! Everything else in that scenario is production code.

#![allow(dead_code)]

use std::sync::Mutex;

use jarvis_application::ports::{ArtifactStore, AuditLog, RepositoryError};
use jarvis_domain::artifact::{ArtifactManifest, ArtifactVersion};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::ids::ArtifactId;

#[derive(Default)]
pub struct FakeArtifactStore {
    manifests: Mutex<Vec<ArtifactManifest>>,
}

impl FakeArtifactStore {
    pub fn with(manifest: ArtifactManifest) -> Self {
        Self {
            manifests: Mutex::new(vec![manifest]),
        }
    }
}

#[async_trait::async_trait]
impl ArtifactStore for FakeArtifactStore {
    async fn create_version(
        &self,
        manifest: &ArtifactManifest,
        _audit: &AuditEvent,
    ) -> Result<(), RepositoryError> {
        self.manifests
            .lock()
            .expect("not poisoned")
            .push(manifest.clone());
        Ok(())
    }

    async fn get(
        &self,
        id: &ArtifactId,
        version: ArtifactVersion,
    ) -> Result<Option<ArtifactManifest>, RepositoryError> {
        Ok(self
            .manifests
            .lock()
            .expect("not poisoned")
            .iter()
            .find(|m| m.id() == id && m.version() == version)
            .cloned())
    }

    async fn latest(&self, id: &ArtifactId) -> Result<Option<ArtifactManifest>, RepositoryError> {
        Ok(self
            .manifests
            .lock()
            .expect("not poisoned")
            .iter()
            .rfind(|m| m.id() == id)
            .cloned())
    }

    async fn list_versions(
        &self,
        id: &ArtifactId,
    ) -> Result<Vec<ArtifactManifest>, RepositoryError> {
        Ok(self
            .manifests
            .lock()
            .expect("not poisoned")
            .iter()
            .filter(|m| m.id() == id)
            .cloned()
            .collect())
    }
}

#[derive(Default)]
pub struct FakeAuditLog {
    pub events: Mutex<Vec<AuditEvent>>,
}

#[async_trait::async_trait]
impl AuditLog for FakeAuditLog {
    async fn record(&self, audit: &AuditEvent) -> Result<(), RepositoryError> {
        self.events
            .lock()
            .expect("not poisoned")
            .push(audit.clone());
        Ok(())
    }
}
