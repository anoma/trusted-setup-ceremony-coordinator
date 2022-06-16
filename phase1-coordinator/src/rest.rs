//! REST API endpoints exposed by the [Coordinator](`crate::Coordinator`).

use crate::{
    authentication::{KeyPair, Production, Signature},
    objects::{ContributionInfo, LockedLocators, Task, TrimmedContributionInfo},
    storage::{ContributionLocator, ContributionSignatureLocator, Locator},
    ContributionFileSignature,
    CoordinatorError,
    Participant,
};

use rocket::{
    error,
    get,
    http::{ContentType, Status},
    post,
    response::{Responder, Response},
    serde::{
        json::{self, Json},
        Deserialize,
        Serialize,
    },
    tokio::{sync::RwLock, task},
    Request,
    Shutdown,
    State,
};

use std::{collections::LinkedList, io::Cursor, net::SocketAddr, ops::Deref, sync::Arc, time::Duration};
use thiserror::Error;

use tracing::debug;

#[cfg(debug_assertions)]
pub const UPDATE_TIME: Duration = Duration::from_secs(5);
#[cfg(not(debug_assertions))]
pub const UPDATE_TIME: Duration = Duration::from_secs(60);

type Coordinator = Arc<RwLock<crate::Coordinator>>;

/// Server errors. Also includes errors generated by the managed [Coordinator](`crate::Coordinator`).
#[derive(Error, Debug)]
pub enum ResponseError {
    #[error("Coordinator failed: {0}")]
    CoordinatorError(CoordinatorError),
    #[error("Request's signature is invalid")]
    InvalidSignature,
    #[error("Io Error: {0}")]
    IoError(#[from] std::io::Error),
    #[error("Thread panicked: {0}")]
    RuntimeError(#[from] task::JoinError),
    #[error("Error with Serde: {0}")]
    SerdeError(#[from] serde_json::error::Error),
    #[error("Error while signing the request: {0}")]
    SigningError(String),
    #[error("Error while terminating the ceremony: {0}")]
    ShutdownError(String),
    #[error("The participant {0} is not allowed to access the endpoint {1}")]
    UnauthorizedParticipant(Participant, String),
    #[error("Could not find contributor with public key {0}")]
    UnknownContributor(String),
    #[error("Could not find the provided Task {0} in coordinator state")]
    UnknownTask(Task),
    #[error("Error while verifying a contribution: {0}")]
    VerificationError(String),
}

impl<'r> Responder<'r, 'static> for ResponseError {
    fn respond_to(self, _request: &'r Request<'_>) -> rocket::response::Result<'static> {
        let response = format!("{}", self);
        Response::build()
            .status(Status::InternalServerError)
            .header(ContentType::JSON)
            .sized_body(response.len(), Cursor::new(response))
            .ok()
    }
}

type Result<T> = std::result::Result<T, ResponseError>;

/// A signed incoming request. Contains the pubkey to check the signature. If the
/// request is None the signature is computed on the pubkey itself.
/// Signature must be computed on the hash of the Json encoding of request and relies on
/// the [`Production`] signature scheme
#[derive(Deserialize, Serialize)]
pub struct SignedRequest<T>
where
    T: Serialize,
{
    request: Option<T>,
    signature: String,
    pubkey: String,
}

impl<T: Serialize> Deref for SignedRequest<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        match &self.request {
            Some(t) => t,
            None => panic!("Expected Some not None"),
        }
    }
}

impl<T: Serialize> SignedRequest<T> {
    fn verify(&self) -> Result<()> { //FIXME: could this take the entire Json<SignedRequest> to prevent the need of reserialization?
        let mut request = json::to_string(&self.pubkey)?;

        if let Some(ref r) = self.request {
            request.push_str(json::to_string(r)?.as_str());
        }

        // FIXME: verify the hash of the request
        if Production.verify(self.pubkey.as_str(), request.as_str(), self.signature.as_str()) {
            Ok(())
        } else {
            Err(ResponseError::InvalidSignature)
        }
    }

    /// Check the signature of the request and also that the request comes from the
    /// [Coordinator](`crate::Coordinator`) itself.
    async fn check_coordinator_request(&self, coordinator: &Coordinator, endpoint: &str) -> Result<()>
    where
        T: Serialize,
    {
        // Check pubkey is the one of the coordinator's verifier
        let verifier = Participant::new_verifier(self.pubkey.as_ref());

        if verifier != coordinator.read().await.environment().coordinator_verifiers()[0] {
            return Err(ResponseError::UnauthorizedParticipant(verifier, endpoint.to_string()));
        }
        // Check signature
        self.verify()
    }

    /// Returns a signed request
    pub fn try_sign(keypair: &KeyPair, request: Option<T>) -> Result<Self> {
        let mut message = json::to_string(&keypair.pubkey().to_owned())?;
        // FIXME: is it correct to concatenate the strings? Better to create a Value?
        // FIXME: sign the hash of the json encoding string (use sha2)
        // If body is non-empty add it to the message to be signed
        if let Some(ref r) = request {
            message.push_str(json::to_string(r)?.as_str());
        }

        match Production.sign(keypair.sigkey(), message.as_str()) {
            Ok(signature) => Ok(SignedRequest {
                request,
                signature,
                pubkey: keypair.pubkey().to_owned(),
            }),
            Err(e) => Err(ResponseError::SigningError(format!("{}", e))),
        }
    }
}

/// The status of the contributor related to the current round.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum ContributorStatus {
    Queue(u64, u64),
    Round,
    Finished,
    Other,
}

/// Request to post a [Chunk](`crate::objects::Chunk`).
#[derive(Clone, Deserialize, Serialize)]
pub struct PostChunkRequest {
    contribution_locator: ContributionLocator,
    contribution: Vec<u8>,
    contribution_file_signature_locator: ContributionSignatureLocator,
    contribution_file_signature: ContributionFileSignature,
}

impl PostChunkRequest {
    pub fn new(
        contribution_locator: ContributionLocator,
        contribution: Vec<u8>,
        contribution_file_signature_locator: ContributionSignatureLocator,
        contribution_file_signature: ContributionFileSignature,
    ) -> Self {
        Self {
            contribution_locator,
            contribution,
            contribution_file_signature_locator,
            contribution_file_signature,
        }
    }
}

//
// -- REST API ENDPOINTS --
//

/// Add the incoming contributor to the queue of contributors.
#[post("/contributor/join_queue", format = "json", data = "<request>")]
pub async fn join_queue(
    coordinator: &State<Coordinator>,
    request: Json<SignedRequest<()>>,
    contributor_ip: SocketAddr,
) -> Result<()> {
    let signed_request = request.into_inner();

    // Check signature
    signed_request.verify()?;

    let contributor = Participant::new_contributor(signed_request.pubkey.as_str());

    let mut write_lock = (*coordinator).clone().write_owned().await;

    match task::spawn_blocking(move || write_lock.add_to_queue(contributor, Some(contributor_ip.ip()), 10)).await? {
        Ok(()) => Ok(()),
        Err(e) => Err(ResponseError::CoordinatorError(e)),
    }
}

/// Lock a [Chunk](`crate::objects::Chunk`) in the ceremony. This should be the first function called when attempting to contribute to a chunk. Once the chunk is locked, it is ready to be downloaded.
#[post("/contributor/lock_chunk", format = "json", data = "<request>")]
pub async fn lock_chunk(
    coordinator: &State<Coordinator>,
    request: Json<SignedRequest<()>>,
) -> Result<Json<LockedLocators>> {
    let signed_request = request.into_inner();

    // Check signature
    signed_request.verify()?;

    let contributor = Participant::new_contributor(signed_request.pubkey.as_str());

    let mut write_lock = (*coordinator).clone().write_owned().await;

    match task::spawn_blocking(move || write_lock.try_lock(&contributor)).await? {
        Ok((_, locked_locators)) => Ok(Json(locked_locators)),
        Err(e) => Err(ResponseError::CoordinatorError(e)),
    }
}

/// Download a chunk from the [Coordinator](`crate::Coordinator`), which should be contributed to upon receipt.
#[get("/download/chunk", format = "json", data = "<get_chunk_request>")]
pub async fn get_chunk(
    coordinator: &State<Coordinator>,
    get_chunk_request: Json<SignedRequest<LockedLocators>>,
) -> Result<Json<Task>> {
    let signed_request = get_chunk_request.into_inner();

    // Check signature
    signed_request.verify()?;

    let contributor = Participant::new_contributor(signed_request.pubkey.as_ref());
    let next_contribution = signed_request.next_contribution();

    // Build and check next Task
    let task = Task::new(next_contribution.chunk_id(), next_contribution.contribution_id());

    let read_lock = (*coordinator).clone().read_owned().await;

    match task::spawn_blocking(move || read_lock.state().current_participant_info(&contributor).cloned()).await? {
        Some(info) => {
            if !info.pending_tasks().contains(&task) {
                return Err(ResponseError::UnknownTask(task));
            }
            Ok(Json(task))
        }
        None => Err(ResponseError::UnknownContributor(signed_request.pubkey)),
    }
}

/// Download the challenge from the [Coordinator](`crate::Coordinator`) accordingly to the [`LockedLocators`] received from the Contributor.
#[get("/contributor/challenge", format = "json", data = "<locked_locators>")]
pub async fn get_challenge(
    coordinator: &State<Coordinator>,
    locked_locators: Json<SignedRequest<LockedLocators>>,
) -> Result<Json<Vec<u8>>> {
    let signed_request = locked_locators.into_inner();

    // Check signature
    signed_request.verify()?;

    let challenge_locator = signed_request.current_contribution();
    let round_height = challenge_locator.round_height();
    let chunk_id = challenge_locator.chunk_id();

    debug!(
        "rest::get_challenge - round_height {}, chunk_id {}, contribution_id 0, is_verified true",
        round_height, chunk_id
    );

    let mut write_lock = (*coordinator).clone().write_owned().await;

    // Since we don't chunk the parameters, we have one chunk and one allowed contributor per round. Thus the challenge will always be located at round_{i}/chunk_0/contribution_0.verified
    // For example, the 1st challenge (after the initialization) is located at round_1/chunk_0/contribution_0.verified
    match task::spawn_blocking(move || write_lock.get_challenge(round_height, chunk_id, 0, true)).await? {
        Ok(challenge_hash) => Ok(Json(challenge_hash)),
        Err(e) => Err(ResponseError::CoordinatorError(e)),
    }
}

/// Upload a [Chunk](`crate::objects::Chunk`) contribution to the [Coordinator](`crate::Coordinator`). Write the contribution bytes to
/// disk at the provided [Locator](`crate::storage::Locator`). Also writes the corresponding [`ContributionFileSignature`]
#[post("/upload/chunk", format = "json", data = "<post_chunk_request>")]
pub async fn post_contribution_chunk(
    coordinator: &State<Coordinator>,
    post_chunk_request: Json<SignedRequest<PostChunkRequest>>,
) -> Result<()> {
    let signed_request = post_chunk_request.into_inner();

    // Check signature
    signed_request.verify()?;

    let request = signed_request.request.unwrap();
    let request_clone = request.clone();
    let mut write_lock = (*coordinator).clone().write_owned().await;

    if let Err(e) =
        task::spawn_blocking(move || write_lock.write_contribution(request.contribution_locator, request.contribution))
            .await?
    {
        return Err(ResponseError::CoordinatorError(e));
    }

    write_lock = (*coordinator).clone().write_owned().await;
    match task::spawn_blocking(move || {
        write_lock.write_contribution_file_signature(
            request_clone.contribution_file_signature_locator,
            request_clone.contribution_file_signature,
        )
    })
    .await?
    {
        Ok(()) => Ok(()),
        Err(e) => Err(ResponseError::CoordinatorError(e)),
    }
}

/// Notify the [Coordinator](`crate::Coordinator`) of a finished and uploaded [Contribution](`crate::objects::Contribution`). This will unlock the given [Chunk](`crate::objects::Chunk`) and allow the contributor to take on a new task.
#[post(
    "/contributor/contribute_chunk",
    format = "json",
    data = "<contribute_chunk_request>"
)]
pub async fn contribute_chunk(
    coordinator: &State<Coordinator>,
    contribute_chunk_request: Json<SignedRequest<u64>>,
) -> Result<Json<ContributionLocator>> {
    let signed_request = contribute_chunk_request.into_inner();

    // Check signature
    signed_request.verify()?;

    let chunk_id = signed_request.request.unwrap();
    let contributor = Participant::new_contributor(signed_request.pubkey.as_ref());

    let mut write_lock = (*coordinator).clone().write_owned().await;

    match task::spawn_blocking(move || write_lock.try_contribute(&contributor, chunk_id)).await? {
        Ok(contribution_locator) => Ok(Json(contribution_locator)),
        Err(e) => Err(ResponseError::CoordinatorError(e)),
    }
}

/// Performs the update of the [Coordinator](`crate::Coordinator`)
pub async fn perform_coordinator_update(coordinator: Coordinator) -> Result<()> {
    let mut write_lock = coordinator.clone().write_owned().await;

    match task::spawn_blocking(move || write_lock.update()).await? {
        Ok(()) => Ok(()),
        Err(e) => Err(ResponseError::CoordinatorError(e)),
    }
}

/// Update the [Coordinator](`crate::Coordinator`) state. This endpoint is accessible only by the coordinator itself.
#[cfg(debug_assertions)]
#[get("/update", format = "json", data = "<request>")]
pub async fn update_coordinator(coordinator: &State<Coordinator>, request: Json<SignedRequest<()>>) -> Result<()> {
    let signed_request = request.into_inner();

    // Verify request
    signed_request.check_coordinator_request(coordinator, "/update").await?;

    perform_coordinator_update(coordinator.deref().to_owned()).await
}

/// Let the [Coordinator](`crate::Coordinator`) know that the participant is still alive and participating (or waiting to participate) in the ceremony.
#[post("/contributor/heartbeat", format = "json", data = "<request>")]
pub async fn heartbeat(coordinator: &State<Coordinator>, request: Json<SignedRequest<()>>) -> Result<()> {
    let signed_request = request.into_inner();

    // Check signature
    signed_request.verify()?;

    let contributor = Participant::new_contributor(signed_request.pubkey.as_str());
    let mut write_lock = (*coordinator).clone().write_owned().await;

    match task::spawn_blocking(move || write_lock.heartbeat(&contributor)).await? {
        Ok(()) => Ok(()),
        Err(e) => Err(ResponseError::CoordinatorError(e)),
    }
}

/// Get the pending tasks of contributor.
#[get("/contributor/get_tasks_left", format = "json", data = "<request>")]
pub async fn get_tasks_left(
    coordinator: &State<Coordinator>,
    request: Json<SignedRequest<()>>,
) -> Result<Json<LinkedList<Task>>> {
    let signed_request = request.into_inner();

    // Check signature
    signed_request.verify()?;

    let contributor = Participant::new_contributor(signed_request.pubkey.as_str());

    let read_lock = (*coordinator).clone().read_owned().await;

    match task::spawn_blocking(move || read_lock.state().current_participant_info(&contributor).cloned()).await? {
        Some(info) => Ok(Json(info.pending_tasks().to_owned())),
        None => Err(ResponseError::UnknownContributor(signed_request.pubkey)),
    }
}

/// Stop the [Coordinator](`crate::Coordinator`) and shuts the server down. This endpoint is accessible only by the coordinator itself.
#[get("/stop", format = "json", data = "<request>")]
pub async fn stop_coordinator(
    coordinator: &State<Coordinator>,
    request: Json<SignedRequest<()>>,
    shutdown: Shutdown,
) -> Result<()> {
    let signed_request = request.into_inner();

    // Verify request
    signed_request.check_coordinator_request(coordinator, "/stop").await?;

    let mut write_lock = (*coordinator).clone().write_owned().await;

    let result = task::spawn_blocking(move || write_lock.shutdown()).await?;

    if let Err(e) = result {
        return Err(ResponseError::ShutdownError(format!("{}", e)));
    };

    // Shut Rocket server down
    shutdown.notify();

    Ok(())
}

/// Performs the verification of the pending contributions
pub async fn perform_verify_chunks(coordinator: Coordinator) -> Result<()> {
    // Get all the pending verifications, loop on each one of them and perform verification
    let pending_verifications = coordinator.read().await.get_pending_verifications().to_owned();

    for (task, _) in pending_verifications {
        let mut write_lock = coordinator.clone().write_owned().await;
        // NOTE: we are going to rely on the single default verifier built in the coordinator itself,
        //  no external verifiers
        if let Err(e) = task::spawn_blocking(move || write_lock.default_verify(&task)).await? {
            return Err(ResponseError::VerificationError(format!("{}", e)));
        }
    }

    Ok(())
}

/// Verify all the pending contributions. This endpoint is accessible only by the coordinator itself.
#[cfg(debug_assertions)]
#[get("/verify", format = "json", data = "<request>")]
pub async fn verify_chunks(coordinator: &State<Coordinator>, request: Json<SignedRequest<()>>) -> Result<()> {
    let signed_request = request.into_inner();

    // Verify request
    signed_request.check_coordinator_request(coordinator, "/verify").await?;

    perform_verify_chunks(coordinator.deref().to_owned()).await
}

/// Get the queue status of the contributor.
#[get("/contributor/queue_status", format = "json", data = "<request>")]
pub async fn get_contributor_queue_status(
    coordinator: &State<Coordinator>,
    request: Json<SignedRequest<()>>,
) -> Result<Json<ContributorStatus>> {
    let signed_request = request.into_inner();

    // Check signature
    signed_request.verify()?;

    let contrib = Participant::new_contributor(signed_request.pubkey.as_str());
    let contributor = contrib.clone();

    let read_lock = (*coordinator).clone().read_owned().await;
    // Check that the contributor is authorized to lock a chunk in the current round.
    if task::spawn_blocking(move || read_lock.is_current_contributor(&contributor)).await? {
        return Ok(Json(ContributorStatus::Round));
    }

    let read_lock = (*coordinator).clone().read_owned().await;
    let coordinator_state = task::spawn_blocking(move || read_lock.state()).await?;

    let read_lock = (*coordinator).clone().read_owned().await;
    let contributor = contrib.clone();

    if task::spawn_blocking(move || read_lock.is_queue_contributor(&contributor)).await? {
        let state = coordinator_state.clone();
        let queue_size = task::spawn_blocking(move || state.number_of_queue_contributors()).await? as u64;
        let contributor = contrib.clone();

        let state = coordinator_state.clone();
        let queue_position =
            match task::spawn_blocking(move || state.queue_contributor_info(&contributor).cloned()).await? {
                Some((_, Some(round), _, _)) => {
                    let state = coordinator_state.clone();
                    round - task::spawn_blocking(move || state.current_round_height()).await?
                }
                Some((_, None, _, _)) => queue_size,
                None => return Ok(Json(ContributorStatus::Other)),
            };

        return Ok(Json(ContributorStatus::Queue(queue_position, queue_size)));
    }

    let read_lock = (*coordinator).clone().read_owned().await;
    let contributor = contrib.clone();
    if task::spawn_blocking(move || read_lock.is_finished_contributor(&contributor)).await? {
        return Ok(Json(ContributorStatus::Finished));
    }

    // Not in the queue, not finished, nor in the current round
    Ok(Json(ContributorStatus::Other))
}

/// Write [`ContributionInfo`] to disk
#[post("/contributor/contribution_info", format = "json", data = "<request>")]
pub async fn post_contribution_info(
    coordinator: &State<Coordinator>,
    request: Json<SignedRequest<ContributionInfo>>,
) -> Result<()> {
    let signed_request = request.into_inner();

    // Check signature
    signed_request.verify()?;

    // Check participant is registered in the ceremony
    let contributor = Participant::new_contributor(signed_request.pubkey.as_str());
    let contributor_clone = contributor.clone();
    let read_lock = (*coordinator).clone().read_owned().await;

    if !task::spawn_blocking(move || {
        read_lock.is_current_contributor(&contributor_clone) || read_lock.is_finished_contributor(&contributor_clone)
    }).await? {
        // Only the current contributor can upload this file
        return Err(ResponseError::UnauthorizedParticipant(
            contributor,
            String::from("/contributor/contribution_info"),
        ));
    }

    // Write contribution info to file
    let contribution_info = signed_request.request.clone().unwrap();
    let mut write_lock = (*coordinator).clone().write_owned().await;
    task::spawn_blocking(move || write_lock.write_contribution_info(contribution_info))
        .await?
        .map_err(|e| ResponseError::CoordinatorError(e))?;

    // Append summary to file
    let contribution_summary = signed_request.request.unwrap().into();
    let mut write_lock = (*coordinator).clone().write_owned().await;
    task::spawn_blocking(move || write_lock.update_contribution_summary(contribution_summary))
        .await?
        .map_err(|e| ResponseError::CoordinatorError(e))?;

    Ok(())
}

/// Retrieve the contributions' info. This endpoint is accessible by anyone and does not require a signed request.
#[get("/contribution_info", format = "json")]
pub async fn get_contributions_info(
    coordinator: &State<Coordinator>,
) -> Result<Json<Vec<TrimmedContributionInfo>>> {
    let read_lock = (*coordinator).clone().read_owned().await;
    let summary = match task::spawn_blocking(move || read_lock.storage().get(&Locator::ContributionsInfoSummary))
        .await?
        .map_err(|e| ResponseError::CoordinatorError(e))?
    {
        crate::storage::Object::ContributionsInfoSummary(summary) => summary,
        _ => unreachable!(),
    };

    Ok(Json(summary))
}

#[cfg(test)]
mod tests_signed_request {
    use super::SignedRequest;
    use crate::authentication::KeyPair;

    #[test]
    fn sign_and_verify() {
        let keypair = KeyPair::new();

        // Empty body
        let request = SignedRequest::<()>::try_sign(&keypair, None).unwrap();
        assert!(request.verify().is_ok());

        // Non-empty body
        let request = SignedRequest::<String>::try_sign(&keypair, Some(String::from("test_body"))).unwrap();
        assert!(request.verify().is_ok());
    }
}
