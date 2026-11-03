//! An in-process gateway on a loopback port serving the provider and provider-profile RPCs the
//! way the OpenShell gateway does: import is create-only and reports a taken id as an
//! `already exists` diagnostic, update is guarded by the stored resource version and answers a
//! stale one with `Aborted`, `CreateProvider` answers a taken name with `AlreadyExists`, and a
//! profile is stored the way the gateway's import normalizes it.

use crate::openshell::grpc::Gateway;
use openshell_core::proto::{
    CreateProviderRequest, GetProviderProfileRequest, GetProviderRequest,
    ImportProviderProfilesRequest, ImportProviderProfilesResponse, Provider,
    ProviderCredentialTokenGrantType, ProviderProfile, ProviderProfileDiagnostic,
    ProviderProfileResponse, ProviderResponse, UpdateProviderProfilesRequest,
    UpdateProviderProfilesResponse, UpdateProviderRequest,
};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tonic::transport::Endpoint;

pub(crate) struct Unary<F>(pub(crate) F);

impl<Req, Resp, F, Fut> tonic::server::UnaryService<Req> for Unary<F>
where
    F: FnMut(tonic::Request<Req>) -> Fut,
    Fut: std::future::Future<Output = Result<tonic::Response<Resp>, tonic::Status>>,
{
    type Response = Resp;
    type Future = Fut;
    fn call(&mut self, request: tonic::Request<Req>) -> Fut {
        (self.0)(request)
    }
}

pub(crate) async fn unary<Req, Resp, S>(
    service: S,
    req: tonic::codegen::http::Request<tonic::body::Body>,
) -> tonic::codegen::http::Response<tonic::body::Body>
where
    Req: tonic_prost::prost::Message + Default + Send + 'static,
    Resp: tonic_prost::prost::Message + Send + 'static,
    S: tonic::server::UnaryService<Req, Response = Resp>,
{
    tonic::server::Grpc::new(tonic_prost::ProstCodec::<Resp, Req>::default())
        .unary(service, req)
        .await
}

/// The writes the gateway applied or refused, for asserting how callers converged.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Writes {
    pub(crate) profile_imports: u32,
    pub(crate) profile_updates: u32,
    /// Profile updates refused for a stale resource version.
    pub(crate) profile_conflicts: u32,
    pub(crate) provider_creates: u32,
    pub(crate) provider_updates: u32,
}

#[derive(Default)]
struct Store {
    profiles: HashMap<String, ProviderProfile>,
    providers: HashSet<String>,
    writes: Writes,
}

#[derive(Clone, Default)]
pub(crate) struct ProfileGateway {
    store: Arc<Mutex<Store>>,
    /// Profile reads still to be held until this many are in flight together.
    held_reads: Arc<AtomicUsize>,
    read_barrier: Option<Arc<tokio::sync::Barrier>>,
    /// Another writer edits every profile right after each read, so no update ever lands.
    rival: bool,
    /// Imports fail validation and write nothing.
    reject_imports: bool,
}

impl ProfileGateway {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Hold the first `n` profile reads until all `n` are in flight, so `n` concurrent callers
    /// all decide their write from the same stored state.
    pub(crate) fn hold_first_reads(mut self, n: usize) -> Self {
        self.held_reads = Arc::new(AtomicUsize::new(n));
        self.read_barrier = Some(Arc::new(tokio::sync::Barrier::new(n)));
        self
    }

    pub(crate) fn with_rival_writer(mut self) -> Self {
        self.rival = true;
        self
    }

    pub(crate) fn rejecting_imports(mut self) -> Self {
        self.reject_imports = true;
        self
    }

    /// Store `profile` as a previous import left it, at resource version 1.
    pub(crate) fn seed_profile(&self, profile: ProviderProfile) {
        let mut stored = stored_form(profile);
        stored.resource_version = 1;
        self.lock().profiles.insert(stored.id.clone(), stored);
    }

    pub(crate) fn seed_provider(&self, name: &str) {
        self.lock().providers.insert(name.to_string());
    }

    pub(crate) fn writes(&self) -> Writes {
        self.lock().writes
    }

    /// The stored profile as `GetProviderProfile` answers it.
    pub(crate) fn profile(&self, id: &str) -> Option<ProviderProfile> {
        self.lock().profiles.get(id).cloned().map(response_form)
    }

    pub(crate) fn has_provider(&self, name: &str) -> bool {
        self.lock().providers.contains(name)
    }

    /// Serve on a loopback port and return a [`Gateway`] dialed at it.
    pub(crate) async fn serve(&self) -> Gateway {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binding a loopback port");
        let addr = listener.local_addr().expect("loopback address");
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(self.clone())
                .serve_with_incoming(tonic::transport::server::TcpIncoming::from(listener)),
        );
        Gateway {
            channel: Endpoint::from_shared(format!("http://{addr}"))
                .expect("loopback endpoint")
                .connect_lazy(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    async fn get_profile(&self, id: String) -> Result<ProviderProfileResponse, tonic::Status> {
        if let Some(barrier) = &self.read_barrier
            && self
                .held_reads
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
        {
            barrier.wait().await;
        }
        let mut store = self.lock();
        let Some(stored) = store.profiles.get_mut(&id) else {
            return Err(tonic::Status::not_found("provider profile not found"));
        };
        let answer = response_form(stored.clone());
        if self.rival {
            stored.description = format!("rival edit {}", stored.resource_version);
            stored.resource_version += 1;
        }
        Ok(ProviderProfileResponse {
            profile: Some(answer),
        })
    }

    fn import(&self, req: ImportProviderProfilesRequest) -> ImportProviderProfilesResponse {
        let mut store = self.lock();
        let mut diagnostics = Vec::new();
        let profiles: Vec<ProviderProfile> =
            req.profiles.into_iter().filter_map(|i| i.profile).collect();
        for p in &profiles {
            if self.reject_imports {
                diagnostics.push(error_diagnostic(
                    &p.id,
                    "endpoints",
                    "rejected by validation",
                ));
            } else if store.profiles.contains_key(&p.id) {
                diagnostics.push(error_diagnostic(
                    &p.id,
                    "id",
                    &format!("custom provider profile '{}' already exists", p.id),
                ));
            }
        }
        if !diagnostics.is_empty() {
            return ImportProviderProfilesResponse {
                diagnostics,
                ..Default::default()
            };
        }
        let mut imported = Vec::new();
        for p in profiles {
            let mut stored = stored_form(p);
            stored.resource_version = 1;
            imported.push(response_form(stored.clone()));
            store.profiles.insert(stored.id.clone(), stored);
            store.writes.profile_imports += 1;
        }
        ImportProviderProfilesResponse {
            profiles: imported,
            imported: true,
            ..Default::default()
        }
    }

    fn update(
        &self,
        req: UpdateProviderProfilesRequest,
    ) -> Result<UpdateProviderProfilesResponse, tonic::Status> {
        let mut store = self.lock();
        let Some(current) = store.profiles.get(&req.id).map(|p| p.resource_version) else {
            return Err(tonic::Status::not_found("provider profile not found"));
        };
        if current != req.expected_resource_version {
            store.writes.profile_conflicts += 1;
            return Err(tonic::Status::aborted(format!(
                "provider profile was modified concurrently (current resource_version: {current})"
            )));
        }
        let mut stored = stored_form(req.profile.and_then(|i| i.profile).unwrap_or_default());
        stored.resource_version = current + 1;
        let answer = response_form(stored.clone());
        store.profiles.insert(req.id, stored);
        store.writes.profile_updates += 1;
        Ok(UpdateProviderProfilesResponse {
            profile: Some(answer),
            updated: true,
            ..Default::default()
        })
    }

    fn create_provider(
        &self,
        req: CreateProviderRequest,
    ) -> Result<ProviderResponse, tonic::Status> {
        let provider = req.provider.unwrap_or_default();
        let name = provider_name(&provider);
        let mut store = self.lock();
        if !store.providers.insert(name) {
            return Err(tonic::Status::already_exists("provider already exists"));
        }
        store.writes.provider_creates += 1;
        Ok(ProviderResponse {
            provider: Some(provider),
            ..Default::default()
        })
    }

    fn get_provider(&self, req: GetProviderRequest) -> Result<ProviderResponse, tonic::Status> {
        if !self.lock().providers.contains(&req.name) {
            return Err(tonic::Status::not_found("provider not found"));
        }
        Ok(ProviderResponse {
            provider: Some(Provider::default()),
            ..Default::default()
        })
    }

    fn update_provider(
        &self,
        req: UpdateProviderRequest,
    ) -> Result<ProviderResponse, tonic::Status> {
        let provider = req.provider.unwrap_or_default();
        let mut store = self.lock();
        if !store.providers.contains(&provider_name(&provider)) {
            return Err(tonic::Status::not_found("provider not found"));
        }
        store.writes.provider_updates += 1;
        Ok(ProviderResponse {
            provider: Some(provider),
            ..Default::default()
        })
    }
}

fn provider_name(provider: &Provider) -> String {
    provider
        .metadata
        .as_ref()
        .map(|m| m.name.clone())
        .unwrap_or_default()
}

fn error_diagnostic(id: &str, field: &str, message: &str) -> ProviderProfileDiagnostic {
    ProviderProfileDiagnostic {
        profile_id: id.to_string(),
        field: field.to_string(),
        message: message.to_string(),
        severity: "error".to_string(),
        ..Default::default()
    }
}

/// The profile as the gateway's import stores it: gateway-owned fields cleared, and an
/// unspecified token-grant type resolved to the client-credentials grant it means.
fn stored_form(mut profile: ProviderProfile) -> ProviderProfile {
    profile.resource_version = 0;
    profile.source.clear();
    profile.scope.clear();
    for grant in profile
        .credentials
        .iter_mut()
        .filter_map(|c| c.token_grant.as_mut())
    {
        if grant.grant_type == ProviderCredentialTokenGrantType::Unspecified as i32 {
            grant.grant_type = ProviderCredentialTokenGrantType::ClientCredentials as i32;
        }
    }
    profile
}

fn response_form(mut stored: ProviderProfile) -> ProviderProfile {
    stored.source = "user".to_string();
    stored.scope = "workspace".to_string();
    stored
}

impl tonic::server::NamedService for ProfileGateway {
    const NAME: &'static str = "openshell.v1.OpenShell";
}

impl tonic::codegen::Service<tonic::codegen::http::Request<tonic::body::Body>> for ProfileGateway {
    type Response = tonic::codegen::http::Response<tonic::body::Body>;
    type Error = std::convert::Infallible;
    type Future = tonic::codegen::BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: tonic::codegen::http::Request<tonic::body::Body>) -> Self::Future {
        let this = self.clone();
        Box::pin(async move {
            Ok(match req.uri().path() {
                "/openshell.v1.OpenShell/GetProviderProfile" => {
                    unary(
                        Unary(move |r: tonic::Request<GetProviderProfileRequest>| {
                            let this = this.clone();
                            async move {
                                this.get_profile(r.into_inner().id)
                                    .await
                                    .map(tonic::Response::new)
                            }
                        }),
                        req,
                    )
                    .await
                }
                "/openshell.v1.OpenShell/ImportProviderProfiles" => {
                    unary(
                        Unary(move |r: tonic::Request<ImportProviderProfilesRequest>| {
                            let resp = this.import(r.into_inner());
                            async move { Ok(tonic::Response::new(resp)) }
                        }),
                        req,
                    )
                    .await
                }
                "/openshell.v1.OpenShell/UpdateProviderProfiles" => {
                    unary(
                        Unary(move |r: tonic::Request<UpdateProviderProfilesRequest>| {
                            let resp = this.update(r.into_inner());
                            async move { resp.map(tonic::Response::new) }
                        }),
                        req,
                    )
                    .await
                }
                "/openshell.v1.OpenShell/CreateProvider" => {
                    unary(
                        Unary(move |r: tonic::Request<CreateProviderRequest>| {
                            let resp = this.create_provider(r.into_inner());
                            async move { resp.map(tonic::Response::new) }
                        }),
                        req,
                    )
                    .await
                }
                "/openshell.v1.OpenShell/GetProvider" => {
                    unary(
                        Unary(move |r: tonic::Request<GetProviderRequest>| {
                            let resp = this.get_provider(r.into_inner());
                            async move { resp.map(tonic::Response::new) }
                        }),
                        req,
                    )
                    .await
                }
                "/openshell.v1.OpenShell/UpdateProvider" => {
                    unary(
                        Unary(move |r: tonic::Request<UpdateProviderRequest>| {
                            let resp = this.update_provider(r.into_inner());
                            async move { resp.map(tonic::Response::new) }
                        }),
                        req,
                    )
                    .await
                }
                _ => tonic::Status::unimplemented(req.uri().path().to_string()).into_http(),
            })
        })
    }
}
