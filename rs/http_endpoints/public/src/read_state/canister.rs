use super::{parse_principal_id, verify_principal_ids};
use crate::{
    common::{cbor_response, into_cbor, make_plaintext_response, remove_effective_principal_id},
    metrics::LABEL_UNKNOWN,
    state_reader_executor::StateReaderExecutor,
    types::ApiReqType,
    validator_executor::ValidatorExecutor,
    HttpError, HttpHandlerMetrics, ReplicaHealthStatus,
};
use bytes::Bytes;
use crossbeam::atomic::AtomicCell;
use http::Request;
use hyper::{Body, Response, StatusCode};
use ic_crypto_interfaces_sig_verification::IngressSigVerifier;
use ic_crypto_tree_hash::{sparse_labeled_tree_from_paths, Label, Path, TooLongPathError};
use ic_interfaces_registry::RegistryClient;
use ic_interfaces_state_manager::StateReader;
use ic_logger::{error, replica_logger::no_op_logger, ReplicaLogger};
use ic_metrics::MetricsRegistry;
use ic_replicated_state::{canister_state::execution_state::CustomSectionType, ReplicatedState};
use ic_types::{
    malicious_flags::MaliciousFlags,
    messages::{
        Blob, Certificate, CertificateDelegation, HttpReadStateContent, HttpReadStateResponse,
        HttpRequest, HttpRequestEnvelope, MessageId, ReadState, SignedRequestBytes,
        EXPECTED_MESSAGE_ID_LENGTH,
    },
    CanisterId, PrincipalId, UserId,
};
use ic_validator::CanisterIdSet;
use std::convert::{Infallible, TryFrom};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll};
use tower::Service;

#[derive(Clone)]
pub struct CanisterReadStateService {
    log: ReplicaLogger,
    metrics: HttpHandlerMetrics,
    health_status: Arc<AtomicCell<ReplicaHealthStatus>>,
    delegation_from_nns: Arc<RwLock<Option<CertificateDelegation>>>,
    state_reader_executor: StateReaderExecutor,
    validator_executor: ValidatorExecutor<ReadState>,
    registry_client: Arc<dyn RegistryClient>,
}

pub struct CanisterReadStateServiceBuilder {
    log: Option<ReplicaLogger>,
    metrics: Option<HttpHandlerMetrics>,
    health_status: Option<Arc<AtomicCell<ReplicaHealthStatus>>>,
    malicious_flags: Option<MaliciousFlags>,
    delegation_from_nns: Arc<RwLock<Option<CertificateDelegation>>>,
    state_reader: Arc<dyn StateReader<State = ReplicatedState>>,
    ingress_verifier: Arc<dyn IngressSigVerifier + Send + Sync>,
    registry_client: Arc<dyn RegistryClient>,
}

impl CanisterReadStateServiceBuilder {
    pub fn builder(
        state_reader: Arc<dyn StateReader<State = ReplicatedState>>,
        registry_client: Arc<dyn RegistryClient>,
        ingress_verifier: Arc<dyn IngressSigVerifier + Send + Sync>,
        delegation_from_nns: Arc<RwLock<Option<CertificateDelegation>>>,
    ) -> Self {
        Self {
            log: None,
            metrics: None,
            health_status: None,
            malicious_flags: None,
            delegation_from_nns,
            state_reader,
            ingress_verifier,
            registry_client,
        }
    }

    pub fn with_logger(mut self, log: ReplicaLogger) -> Self {
        self.log = Some(log);
        self
    }

    pub(crate) fn with_malicious_flags(mut self, malicious_flags: MaliciousFlags) -> Self {
        self.malicious_flags = Some(malicious_flags);
        self
    }

    pub fn with_health_status(
        mut self,
        health_status: Arc<AtomicCell<ReplicaHealthStatus>>,
    ) -> Self {
        self.health_status = Some(health_status);
        self
    }

    pub(crate) fn with_metrics(mut self, metrics: HttpHandlerMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    pub fn build(self) -> CanisterReadStateService {
        let log = self.log.unwrap_or(no_op_logger());
        let default_metrics_registry = MetricsRegistry::default();
        CanisterReadStateService {
            log: log.clone(),
            metrics: self
                .metrics
                .unwrap_or_else(|| HttpHandlerMetrics::new(&default_metrics_registry)),
            health_status: self
                .health_status
                .unwrap_or_else(|| Arc::new(AtomicCell::new(ReplicaHealthStatus::Healthy))),
            delegation_from_nns: self.delegation_from_nns,
            state_reader_executor: StateReaderExecutor::new(self.state_reader),
            validator_executor: ValidatorExecutor::new(
                self.registry_client.clone(),
                self.ingress_verifier,
                &self.malicious_flags.unwrap_or_default(),
                log,
            ),
            registry_client: self.registry_client,
        }
    }
}

impl Service<Request<Bytes>> for CanisterReadStateService {
    type Response = Response<Body>;
    type Error = Infallible;
    #[allow(clippy::type_complexity)]
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request<Bytes>) -> Self::Future {
        self.metrics
            .request_body_size_bytes
            .with_label_values(&[ApiReqType::ReadState.into(), LABEL_UNKNOWN])
            .observe(request.body().len() as f64);

        if self.health_status.load() != ReplicaHealthStatus::Healthy {
            let res = make_plaintext_response(
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "Replica is unhealthy: {}. Check the /api/v2/status for more information.",
                    self.health_status.load(),
                ),
            );
            return Box::pin(async move { Ok(res) });
        }
        let (mut parts, body) = request.into_parts();
        // By removing the principal id we get ownership and avoid having to clone it when creating the future.
        let effective_principal_id = match remove_effective_principal_id(&mut parts) {
            Ok(canister_id) => canister_id,
            Err(res) => {
                error!(
                    self.log,
                    "Effective principal ID is not attached to read state request. This is a bug."
                );
                return Box::pin(async move { Ok(res) });
            }
        };

        let delegation_from_nns = self.delegation_from_nns.read().unwrap().clone();

        let request = match <HttpRequestEnvelope<HttpReadStateContent>>::try_from(
            &SignedRequestBytes::from(body.to_vec()),
        ) {
            Ok(request) => request,
            Err(e) => {
                let res = make_plaintext_response(
                    StatusCode::BAD_REQUEST,
                    format!("Could not parse body as read request: {}", e),
                );
                return Box::pin(async move { Ok(res) });
            }
        };

        // Convert the message to a strongly-typed struct.
        let request = match HttpRequest::<ReadState>::try_from(request) {
            Ok(request) => request,
            Err(e) => {
                let res = make_plaintext_response(
                    StatusCode::BAD_REQUEST,
                    format!("Malformed request: {:?}", e),
                );
                return Box::pin(async move { Ok(res) });
            }
        };

        let read_state = request.content().clone();
        let registry_version = self.registry_client.get_latest_version();
        let state_reader_executor = self.state_reader_executor.clone();
        let validator_executor = self.validator_executor.clone();
        let metrics = self.metrics.clone();
        Box::pin(async move {
            let targets_fut =
                validator_executor.validate_request(request.clone(), registry_version);

            let targets = match targets_fut.await {
                Ok(targets) => targets,
                Err(http_err) => {
                    let res = make_plaintext_response(http_err.status, http_err.message);
                    return Ok(res);
                }
            };
            let make_service_unavailable_response = || {
                make_plaintext_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Certified state is not available yet. Please try again...".to_string(),
                )
            };
            let certified_state_reader =
                match state_reader_executor.get_certified_state_snapshot().await {
                    Ok(Some(reader)) => reader,
                    Ok(None) => return Ok(make_service_unavailable_response()),
                    Err(HttpError { status, message }) => {
                        return Ok(make_plaintext_response(status, message))
                    }
                };

            // Verify authorization for requested paths.
            if let Err(HttpError { status, message }) = verify_paths(
                certified_state_reader.get_state(),
                &read_state.source,
                &read_state.paths,
                &targets,
                effective_principal_id,
            ) {
                return Ok(make_plaintext_response(status, message));
            }

            // Create labeled tree. This may be an expensive operation and by
            // creating the labeled tree after verifying the paths we know that
            // the depth is max 4.
            // Always add "time" to the paths even if not explicitly requested.
            let mut paths: Vec<Path> = read_state.paths;
            paths.push(Path::from(Label::from("time")));
            let labeled_tree = match sparse_labeled_tree_from_paths(&paths) {
                Ok(tree) => tree,
                Err(TooLongPathError) => {
                    let res = make_plaintext_response(
                        StatusCode::BAD_REQUEST,
                        "Failed to parse requested paths: path is too long.".to_string(),
                    );
                    return Ok(res);
                }
            };

            let (tree, certification) =
                match certified_state_reader.read_certified_state(&labeled_tree) {
                    Some(r) => r,
                    None => return Ok(make_service_unavailable_response()),
                };

            let signature = certification.signed.signature.signature.get().0;
            let res = HttpReadStateResponse {
                certificate: Blob(into_cbor(&Certificate {
                    tree,
                    signature: Blob(signature),
                    delegation: delegation_from_nns,
                })),
            };
            let (resp, body_size) = cbor_response(&res);
            metrics
                .response_body_size_bytes
                .with_label_values(&[ApiReqType::ReadState.into()])
                .observe(body_size as f64);
            Ok(resp)
        })
    }
}

// Verifies that the `user` is authorized to retrieve the `paths` requested.
fn verify_paths(
    state: &ReplicatedState,
    user: &UserId,
    paths: &[Path],
    targets: &CanisterIdSet,
    effective_principal_id: PrincipalId,
) -> Result<(), HttpError> {
    let mut request_status_id: Option<MessageId> = None;

    // Convert the paths to slices to make it easier to match below.
    let paths: Vec<Vec<&[u8]>> = paths
        .iter()
        .map(|path| path.iter().map(|label| label.as_bytes()).collect())
        .collect();

    for path in paths {
        match path.as_slice() {
            [b"time"] => {}
            [b"canister", canister_id, b"controllers" | b"module_hash"] => {
                let canister_id = parse_principal_id(canister_id)?;
                verify_principal_ids(&canister_id, &effective_principal_id)?;
            }
            [b"canister", canister_id, b"metadata", name] => {
                let name = String::from_utf8(Vec::from(*name)).map_err(|err| HttpError {
                    status: StatusCode::BAD_REQUEST,
                    message: format!("Could not parse the custom section name: {}.", err),
                })?;

                // Get principal id from byte slice.
                let principal_id = parse_principal_id(canister_id)?;
                // Verify that canister id and effective canister id match.
                verify_principal_ids(&principal_id, &effective_principal_id)?;
                can_read_canister_metadata(
                    user,
                    &CanisterId::unchecked_from_principal(principal_id),
                    &name,
                    state,
                )?
            }
            [b"subnet"] => {}
            [b"subnet", _subnet_id]
            | [b"subnet", _subnet_id, b"public_key" | b"canister_ranges" | b"node"] => {}
            [b"subnet", _subnet_id, b"node", _node_id]
            | [b"subnet", _subnet_id, b"node", _node_id, b"public_key"] => {}
            [b"request_status", request_id]
            | [b"request_status", request_id, b"status" | b"reply" | b"reject_code" | b"reject_message" | b"error_code"] =>
            {
                // Verify that the request was signed by the same user.
                if let Ok(message_id) = MessageId::try_from(*request_id) {
                    if let Some(request_status_id) = request_status_id {
                        if request_status_id != message_id {
                            return Err(HttpError {
                                status: StatusCode::BAD_REQUEST,
                                message:
                                    "Can only request a single request ID in request_status paths."
                                        .to_string(),
                            });
                        }
                    }

                    let ingress_status = state.get_ingress_status(&message_id);
                    if let Some(ingress_user_id) = ingress_status.user_id() {
                        if let Some(receiver) = ingress_status.receiver() {
                            if ingress_user_id != *user {
                                return Err(HttpError {
                                    status: StatusCode::FORBIDDEN,
                                    message:
                                        "Request IDs must be for requests signed by the caller."
                                            .to_string(),
                                });
                            }

                            if !targets.contains(&receiver) {
                                return Err(HttpError {
                                    status: StatusCode::FORBIDDEN,
                                    message:
                                        "Request IDs must be for requests to canisters belonging to sender delegation targets."
                                            .to_string(),
                                });
                            }
                        }
                    }

                    request_status_id = Some(message_id);
                } else {
                    return Err(HttpError {
                        status: StatusCode::BAD_REQUEST,
                        message: format!(
                            "Request IDs must be {} bytes in length.",
                            EXPECTED_MESSAGE_ID_LENGTH
                        ),
                    });
                }
            }
            _ => {
                // All other paths are unsupported.
                return Err(HttpError {
                    status: StatusCode::NOT_FOUND,
                    message: "Invalid path requested.".to_string(),
                });
            }
        }
    }

    Ok(())
}

fn can_read_canister_metadata(
    user: &UserId,
    canister_id: &CanisterId,
    custom_section_name: &str,
    state: &ReplicatedState,
) -> Result<(), HttpError> {
    let canister = match state.canister_states.get(canister_id) {
        Some(canister) => canister,
        None => return Ok(()),
    };

    match &canister.execution_state {
        Some(execution_state) => {
            let custom_section = match execution_state
                .metadata
                .get_custom_section(custom_section_name)
            {
                Some(section) => section,
                None => return Ok(()),
            };

            // Only the controller can request this custom section.
            if custom_section.visibility() == CustomSectionType::Private
                && !canister.system_state.controllers.contains(&user.get())
            {
                return Err(HttpError {
                    status: StatusCode::FORBIDDEN,
                    message: format!(
                        "Custom section {:.100} can only be requested by the controllers of the canister.",
                        custom_section_name
                    ),
                });
            }

            Ok(())
        }
        None => Ok(()),
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{
        common::test::{array, assert_cbor_ser_equal, bytes, int},
        HttpError,
    };
    use hyper::StatusCode;
    use ic_crypto_tree_hash::{Digest, Label, MixedHashTree, Path};
    use ic_registry_subnet_type::SubnetType;
    use ic_replicated_state::{
        canister_snapshots::CanisterSnapshots, CanisterQueues, ReplicatedState, SystemMetadata,
    };
    use ic_test_utilities::{
        state::insert_dummy_canister,
        types::ids::{canister_test_id, subnet_test_id, user_test_id},
    };
    use ic_test_utilities_time::mock_time;
    use ic_types::batch::RawQueryStats;
    use ic_validator::CanisterIdSet;
    use std::collections::BTreeMap;

    #[test]
    fn encoding_read_state_tree_empty() {
        let tree = MixedHashTree::Empty;
        assert_cbor_ser_equal(&tree, array(vec![int(0)]));
    }

    #[test]
    fn encoding_read_state_tree_leaf() {
        let tree = MixedHashTree::Leaf(vec![1, 2, 3]);
        assert_cbor_ser_equal(&tree, array(vec![int(3), bytes(&[1, 2, 3])]));
    }

    #[test]
    fn encoding_read_state_tree_pruned() {
        let tree = MixedHashTree::Pruned(Digest([1; 32]));
        assert_cbor_ser_equal(&tree, array(vec![int(4), bytes(&[1; 32])]));
    }

    #[test]
    fn encoding_read_state_tree_fork() {
        let tree = MixedHashTree::Fork(Box::new((
            MixedHashTree::Leaf(vec![1, 2, 3]),
            MixedHashTree::Leaf(vec![4, 5, 6]),
        )));
        assert_cbor_ser_equal(
            &tree,
            array(vec![
                int(1),
                array(vec![int(3), bytes(&[1, 2, 3])]),
                array(vec![int(3), bytes(&[4, 5, 6])]),
            ]),
        );
    }

    #[test]
    fn encoding_read_state_tree_mixed() {
        let tree = MixedHashTree::Fork(Box::new((
            MixedHashTree::Labeled(
                Label::from(vec![1, 2, 3]),
                Box::new(MixedHashTree::Pruned(Digest([2; 32]))),
            ),
            MixedHashTree::Leaf(vec![4, 5, 6]),
        )));
        assert_cbor_ser_equal(
            &tree,
            array(vec![
                int(1),
                array(vec![
                    int(2),
                    bytes(&[1, 2, 3]),
                    array(vec![int(4), bytes(&[2; 32])]),
                ]),
                array(vec![int(3), bytes(&[4, 5, 6])]),
            ]),
        );
    }

    #[test]
    fn user_can_read_canister_metadata() {
        let canister_id = canister_test_id(100);
        let controller = user_test_id(24);
        let non_controller = user_test_id(20);

        let mut state = ReplicatedState::new(subnet_test_id(1), SubnetType::Application);
        insert_dummy_canister(&mut state, canister_id, controller.get());

        let public_name = "dummy";
        // Controller can read the public custom section
        assert!(can_read_canister_metadata(&controller, &canister_id, public_name, &state).is_ok());

        // Non-controller can read public custom section
        assert!(
            can_read_canister_metadata(&non_controller, &canister_id, public_name, &state).is_ok()
        );

        let private_name = "candid";
        // Controller can read private custom section
        assert!(
            can_read_canister_metadata(&controller, &canister_id, private_name, &state).is_ok()
        );
    }

    #[test]
    fn user_cannot_read_canister_metadata() {
        let canister_id = canister_test_id(100);
        let controller = user_test_id(24);
        let non_controller = user_test_id(20);

        let mut state = ReplicatedState::new(subnet_test_id(1), SubnetType::Application);
        insert_dummy_canister(&mut state, canister_id, controller.get());

        // Non-controller cannot read private custom section named `candid`.
        assert_eq!(
            can_read_canister_metadata(&non_controller, &canister_id, "candid", &state),
            Err(HttpError {
                status: StatusCode::FORBIDDEN,
                message: "Custom section candid can only be requested by the controllers of the canister."
                    .to_string()
            })
        );

        // Non existent public custom section.
        assert_eq!(
            can_read_canister_metadata(&non_controller, &canister_id, "unknown-name", &state),
            Ok(())
        );
    }

    #[test]
    fn test_verify_path() {
        let subnet_id = subnet_test_id(1);
        let mut metadata = SystemMetadata::new(subnet_id, SubnetType::Application);
        metadata.batch_time = mock_time();
        let state = ReplicatedState::new_from_checkpoint(
            BTreeMap::new(),
            metadata,
            CanisterQueues::default(),
            RawQueryStats::default(),
            CanisterSnapshots::default(),
        );
        assert_eq!(
            verify_paths(
                &state,
                &user_test_id(1),
                &[Path::from(Label::from("time"))],
                &CanisterIdSet::all(),
                canister_test_id(1).get(),
            ),
            Ok(())
        );
        assert_eq!(
            verify_paths(
                &state,
                &user_test_id(1),
                &[
                    Path::new(vec![
                        Label::from("request_status"),
                        [0; 32].into(),
                        Label::from("status")
                    ]),
                    Path::new(vec![
                        Label::from("request_status"),
                        [0; 32].into(),
                        Label::from("reply")
                    ])
                ],
                &CanisterIdSet::all(),
                canister_test_id(1).get(),
            ),
            Ok(())
        );
        assert!(verify_paths(
            &state,
            &user_test_id(1),
            &[
                Path::new(vec![Label::from("request_status"), [0; 32].into()]),
                Path::new(vec![Label::from("request_status"), [1; 32].into()])
            ],
            &CanisterIdSet::all(),
            canister_test_id(1).get(),
        )
        .is_err());
    }
}
