//! Capsule reads backed by the same Aster history the commit fence validates.
//!
//! This adapter is the production document store for a Postgres broker. Module
//! bundles may still come from Convex's module tables, but transaction data,
//! snapshot selection, retention, conflict validation, and append all use the
//! single `aster.log` history owned by [`WritePlane`].

use std::sync::Arc;

use aster_broker::{mint_opaque_document_id, CapsuleStore, StoreError};
use aster_capsule::{
    DeploymentId, DocumentId, KeyInterval, RangeCertificate, RangeCertificateError, ScanDirection,
    ScanStop, SnapshotCapsule, TenantId, Timestamp, VersionedDocument,
};

use crate::PostgresCapsuleStore;
use crate::WritePlane;

/// A tenant/deployment-bound read view over an authoritative [`WritePlane`].
///
/// Binding the namespace at construction keeps cell-provided identifiers out
/// of SQL routing. The broker independently checks the capsule namespace; this
/// adapter repeats the check while building a capsule so it stays safe if used
/// outside brokerd.
pub struct AuthoritativeCapsuleStore {
    plane: Arc<WritePlane>,
    tenant: TenantId,
    deployment: DeploymentId,
    id_allocator: Option<Arc<PostgresCapsuleStore>>,
}

impl AuthoritativeCapsuleStore {
    pub fn new(plane: Arc<WritePlane>, tenant: TenantId, deployment: DeploymentId) -> Self {
        Self {
            plane,
            tenant,
            deployment,
            id_allocator: None,
        }
    }

    pub fn with_id_allocator(
        plane: Arc<WritePlane>,
        tenant: TenantId,
        deployment: DeploymentId,
        id_allocator: Arc<PostgresCapsuleStore>,
    ) -> Self {
        Self {
            plane,
            tenant,
            deployment,
            id_allocator: Some(id_allocator),
        }
    }

    fn ensure_namespace(
        &self,
        tenant: &TenantId,
        deployment: &DeploymentId,
    ) -> Result<(), StoreError> {
        if tenant != &self.tenant || deployment != &self.deployment {
            return Err(StoreError::Backend(format!(
                "authoritative store is bound to {}/{}, not {}/{}",
                self.tenant.0, self.deployment.0, tenant.0, deployment.0
            )));
        }
        Ok(())
    }
}

impl CapsuleStore for AuthoritativeCapsuleStore {
    fn snapshot_ts(&self) -> Result<Timestamp, StoreError> {
        self.plane.snapshot_ts(&self.tenant.0, &self.deployment.0)
    }

    fn mint_document_id(&self, table: &str) -> Result<DocumentId, StoreError> {
        match &self.id_allocator {
            Some(allocator) => allocator.mint_document_id(table),
            None => mint_opaque_document_id(table),
        }
    }

    fn read_point(&self, key: &DocumentId, ts: Timestamp) -> Result<VersionedDocument, StoreError> {
        self.plane
            .read_point(&self.tenant.0, &self.deployment.0, key, ts)
    }

    fn scan_prefix(
        &self,
        prefix: &str,
        limit: usize,
        ts: Timestamp,
    ) -> Result<(RangeCertificate, Vec<(DocumentId, VersionedDocument)>), StoreError> {
        if limit == 0 {
            return Err(StoreError::Backend(format!(
                "scan_prefix: {}",
                RangeCertificateError::ZeroLimit
            )));
        }

        let entries =
            self.plane
                .read_prefix_live(&self.tenant.0, &self.deployment.0, prefix, limit, ts)?;
        let stop = if entries.len() == limit {
            ScanStop::Boundary
        } else {
            ScanStop::Exhausted
        };
        let certificate = RangeCertificate {
            interval: KeyInterval::Prefix(prefix.to_string()),
            direction: ScanDirection::Ascending,
            limit: limit as u64,
            keys: entries.iter().map(|(key, _)| key.clone()).collect(),
            stop,
        };
        Ok((certificate, entries))
    }

    fn build_capsule(
        &self,
        tenant: TenantId,
        deployment: DeploymentId,
        ts: Timestamp,
        prewarm: Vec<DocumentId>,
    ) -> Result<SnapshotCapsule, StoreError> {
        self.ensure_namespace(&tenant, &deployment)?;
        let mut capsule = SnapshotCapsule::empty(tenant, deployment, ts);
        for key in prewarm {
            let value = self.read_point(&key, ts)?;
            capsule.hydrate_point(key, value);
        }
        Ok(capsule)
    }
}
