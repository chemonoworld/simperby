pub mod format;
pub mod raw;
mod receive;
mod utils;
// TODO: integrate the server feature with `DistributedRepository`
pub mod server;

use eyre::eyre;
use format::*;
use futures::prelude::*;
use log::{info, warn};
use raw::RawRepository;
use serde::{Deserialize, Serialize};
use simperby_common::reserved::ReservedState;
use simperby_common::utils::get_timestamp;
use simperby_common::verify::CommitSequenceVerifier;
use simperby_common::*;
use simperby_network::{NetworkConfig, Peer, SharedKnownPeers};
use std::{collections::HashSet, fmt};
use utils::{read_commits, retrieve_local_branches};
pub type Branch = String;
pub type Tag = String;

pub const FINALIZED_BRANCH_NAME: &str = "finalized";
pub const WORK_BRANCH_NAME: &str = "work";
pub const FP_BRANCH_NAME: &str = "fp";
pub const COMMIT_TITLE_HASH_DIGITS: usize = 8;
pub const TAG_NAME_HASH_DIGITS: usize = 8;
pub const BRANCH_NAME_HASH_DIGITS: usize = 8;
pub const UNKNOWN_COMMIT_AUTHOR: &str = "unknown";

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Copy, Clone, Hash)]
pub struct CommitHash {
    pub hash: [u8; 20],
}

impl Serialize for CommitHash {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(hex::encode(self.hash).as_str())
    }
}

impl fmt::Display for CommitHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.hash).as_str())
    }
}

impl<'de> Deserialize<'de> for CommitHash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let hash = hex::decode(s).map_err(serde::de::Error::custom)?;
        if hash.len() != 20 {
            return Err(serde::de::Error::custom("invalid length"));
        }
        let mut hash_array = [0; 20];
        hash_array.copy_from_slice(&hash);
        Ok(CommitHash { hash: hash_array })
    }
}

pub type Error = eyre::Error;

#[derive(thiserror::Error, Debug)]
#[error("repository integrity broken: {msg}")]
pub struct IntegrityError {
    pub msg: String,
}

impl IntegrityError {
    pub fn new(msg: String) -> Self {
        Self { msg }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Public repos (usually mirrors) for the read-only accesses
    ///
    /// They're added as a remote repo, named `public_#`.
    /// Note that they are not part of the `known_peers`.
    pub mirrors: Vec<String>,
    /// The distance that if a commit is past this far,
    /// any forked branch starting from the commit
    /// will be considered a long range attack and thus ignored.
    ///
    /// If zero, fork can be detected only from the currently last-finalized commit.
    pub long_range_attack_distance: usize,
}

/// The local Simperby blockchain data repository.
///
/// It automatically locks the repository once created.
///
/// - It **verifies** all the incoming changes and applies them to the local repository
/// only if they are valid.
pub struct DistributedRepository<T> {
    raw: T,
    config: Config,
    peers: SharedKnownPeers,
}

impl<T: RawRepository> DistributedRepository<T> {
    pub fn get_raw_mut(&mut self) -> &mut T {
        &mut self.raw
    }

    pub fn get_raw(&self) -> &T {
        &self.raw
    }

    pub async fn new(raw: T, config: Config, peers: SharedKnownPeers) -> Result<Self, Error> {
        Ok(Self { raw, config, peers })
    }

    /// Initializes the genesis repository, leaving a genesis header.
    ///
    /// It also
    /// - creates `fp` branch and its commit (for the genesis block).
    /// - creates `work` branch at the same place with the `finalized` branch.
    ///
    /// Note that `genesis` can be called on any commit.
    pub async fn genesis(&mut self) -> Result<(), Error> {
        let reserved_state = self.get_reserved_state().await?;
        let block_commit = Commit::Block(reserved_state.genesis_info.header.clone());
        let semantic_commit = to_semantic_commit(&block_commit, reserved_state.clone())?;

        self.raw.checkout_clean().await?;
        // TODO: ignore only if the error is 'already exists'. Otherwise, propagate the error.
        let _ = self
            .raw
            .create_branch(FINALIZED_BRANCH_NAME.into(), self.raw.get_head().await?)
            .await;
        self.raw
            .checkout(FINALIZED_BRANCH_NAME.into())
            .await
            .map_err(|e| match e {
                raw::Error::NotFound(_) => {
                    eyre!(IntegrityError::new(format!(
                        "failed to checkout to the finalized branch: {e}"
                    )))
                }
                _ => eyre!(e),
            })?;
        let result = self.raw.create_semantic_commit(semantic_commit).await?;
        // TODO: ignore only if the error is 'already exists'. Otherwise, propagate the error.
        let _ = self
            .raw
            .create_branch(WORK_BRANCH_NAME.into(), result)
            .await;
        // TODO: ignore only if the error is 'already exists'. Otherwise, propagate the error.
        let _ = self.raw.create_branch(FP_BRANCH_NAME.into(), result).await;
        self.raw
            .checkout(FP_BRANCH_NAME.into())
            .await
            .map_err(|e| match e {
                raw::Error::NotFound(_) => {
                    eyre!(IntegrityError::new(format!(
                        "failed to checkout to the fp branch: {e}"
                    )))
                }
                _ => eyre!(e),
            })?;
        self.raw
            .create_semantic_commit(fp_to_semantic_commit(&LastFinalizationProof {
                height: 0,
                proof: reserved_state.genesis_info.genesis_proof.clone(),
            }))
            .await?;
        Ok(())
    }

    /// Returns the block header from the `finalized` branch.
    pub async fn get_last_finalized_block_header(&self) -> Result<BlockHeader, Error> {
        let commit_hash = self.raw.locate_branch(FINALIZED_BRANCH_NAME.into()).await?;
        let semantic_commit = self.raw.read_semantic_commit(commit_hash).await?;
        let commit = format::from_semantic_commit(semantic_commit).map_err(|e| eyre!(e))?;
        if let Commit::Block(block_header) = commit {
            Ok(block_header)
        } else {
            Err(eyre!(IntegrityError {
                msg: "`finalized` branch is not on a block".to_owned(),
            }))
        }
    }

    pub async fn read_commit(&self, commit_hash: CommitHash) -> Result<Commit, Error> {
        let semantic_commit = self.raw.read_semantic_commit(commit_hash).await?;
        format::from_semantic_commit(semantic_commit).map_err(|e| eyre!(e))
    }

    /// Returns the reserved state from the `finalized` branch.
    pub async fn get_reserved_state(&self) -> Result<ReservedState, Error> {
        self.raw.read_reserved_state().await.map_err(|e| eyre!(e))
    }

    /// Cleans all the outdated commits, remote repositories and branches.
    ///
    /// It will leave only
    /// - the `finalized` branch
    /// - the `work` branch
    /// - the `fp` branch
    /// when `hard` is `true`,
    ///
    /// and when `hard` is `false`,
    /// - the `p` branch
    /// - the `a-#` branches
    /// - the `b-#` branches
    /// will be left as well
    /// if only the branches have valid commit sequences
    /// and are not outdated (branched from the last finalized commit).
    pub async fn clean(&mut self, hard: bool) -> Result<(), Error> {
        let finalized_branch_commit_hash = self
            .raw
            .locate_branch(FINALIZED_BRANCH_NAME.into())
            .await
            .map_err(|e| match e {
                raw::Error::NotFound(_) => {
                    eyre!(IntegrityError::new(
                        "cannot locate `finalized` branch".to_string()
                    ))
                }
                _ => eyre!(e),
            })?;
        let branches = retrieve_local_branches(&self.raw).await?;
        let last_header = self.get_last_finalized_block_header().await?;
        for (branch, branch_commit_hash) in branches {
            if !(branch.as_str() == WORK_BRANCH_NAME
                || branch.as_str() == FINALIZED_BRANCH_NAME
                || branch.as_str() == FP_BRANCH_NAME)
            {
                if hard {
                    self.raw.delete_branch(branch.to_string()).await?;
                } else {
                    // Delete outdated branch
                    let find_merge_base_result = self
                        .raw
                        .find_merge_base(branch_commit_hash, finalized_branch_commit_hash)
                        .await
                        .map_err(|e| match e {
                            raw::Error::NotFound(_) => {
                                eyre!(IntegrityError::new(format!(
                                "cannot find merge base for branch {branch} and finalized branch"
                            )))
                            }
                            _ => eyre!(e),
                        })?;

                    if finalized_branch_commit_hash != find_merge_base_result {
                        self.raw.delete_branch(branch.to_string()).await?;
                    }

                    // Delete branch with invalid commit sequence
                    self.raw.checkout(branch.to_string()).await?;
                    let reserved_state = self.get_reserved_state().await?;
                    let commits =
                        read_commits(self, finalized_branch_commit_hash, branch_commit_hash)
                            .await?;
                    let mut verifier =
                        CommitSequenceVerifier::new(last_header.clone(), reserved_state.clone())
                            .map_err(|e| {
                                eyre!("failed to create a commit sequence verifier: {}", e)
                            })?;
                    for (commit, _) in commits.iter() {
                        if verifier.apply_commit(commit).is_err() {
                            self.raw.delete_branch(branch.to_string()).await?;
                        }
                    }
                }
            }
        }

        // Remove remote repositories
        // Note that remote branches are automatically removed when the remote repository is removed.
        let remote_list = self.raw.list_remotes().await?;
        for (remote_name, _) in remote_list {
            self.raw.remove_remote(remote_name).await?;
        }

        Ok(())
    }

    /// Broadcasts all the local messages.
    pub async fn broadcast(&mut self) -> Result<(), Error> {
        // TODO: perform git push
        Ok(())
    }

    /// Fetches new commits from the network.
    ///
    /// It **verifies** all the incoming changes and applies them to the local repository
    /// only if they are valid.
    ///
    /// - It may move the `finalized` branch.
    /// - It may add some `a-#` branches.
    /// - It may add some `b-#` branches.
    /// - It may update the `fp` branch.
    ///
    /// It may leave some remote repository (representing each peer) after the operation.
    ///
    /// TODO: add fork detection logic considering the long range attack distance.
    pub async fn fetch(&mut self) -> Result<(), Error> {
        utils::add_remotes(self, &self.peers.read().await).await?;
        // TODO: handle this
        let _ = self.raw.fetch_all().await;
        let remote_branches = self.raw.list_remote_tracking_branches().await?;
        for (remote_name, branch_name, commit_hash) in remote_branches {
            let branch_displayed = format!(
                "{}/{}(at {})",
                remote_name,
                branch_name,
                serde_spb::to_string(&commit_hash).unwrap()
            );
            let result = receive::receive(self, commit_hash).await?;
            if let Err(e) = result {
                warn!("failed to apply remote branch {}: {}", branch_displayed, e);
            }
        }
        Ok(())
    }

    /// For a server node, get pushed commits from the network.
    ///
    /// Like [`fetch`], it verifies the incoming change and apply it to the local repository.
    /// Refer to [`fetch`] for more details.
    ///
    /// - Returns `Ok(Ok(()))` if the branch is successfully received.
    /// - Returns `Ok(Err(_))` if the branch is invalid and thus rejected, with the reason.
    /// - Returns `Err(_)` if an error occurs.
    pub async fn get_pushed(
        &mut self,
        commit_hash: CommitHash,
    ) -> Result<Result<(), String>, Error> {
        receive::receive(self, commit_hash).await
    }

    /// Serves the distributed repository protocol indefinitely.
    /// It **verifies** all the incoming changes and applies them to the local repository
    /// only if they are valid.
    pub async fn serve(
        self,
        _network_config: &NetworkConfig,
        _peers: SharedKnownPeers,
    ) -> Result<tokio::task::JoinHandle<Result<(), Error>>, Error> {
        unimplemented!()
    }

    /// Checks the validity of the repository, starting from the given height.
    ///
    /// It checks
    /// 1. all the reserved branches and tags
    /// 2. the finalization proof in the `fp` branch.
    /// 3. the existence of merge commits
    /// 4. the canonical history of the `finalized` branch.
    /// 5. the reserved state in a valid format.
    pub async fn check(&self, _starting_height: BlockHeight) -> Result<bool, Error> {
        unimplemented!()
    }

    /// Synchronizes the `finalized` branch to the given commit.
    ///
    /// This will verify every commit along the way.
    /// If the given commit is not a descendant of the
    /// current `finalized` (i.e., cannot be fast-forwarded), it fails.
    ///
    /// Note that the last block will be verified by the finalization proof
    /// and the `fp` branch will be updated as well.
    pub async fn sync(
        &mut self,
        block_hash: &Hash256,
        last_block_proof: &FinalizationProof,
    ) -> Result<(), Error> {
        let block_branch_name =
            format!("b-{}", &block_hash.to_string()[0..BRANCH_NAME_HASH_DIGITS]);
        let block_commit_hash = self.raw.locate_branch(block_branch_name.clone()).await?;

        if block_commit_hash
            == self
                .raw
                .locate_branch(FINALIZED_BRANCH_NAME.to_owned())
                .await?
        {
            info!("already finalized");
            return Ok(());
        }

        // Check if the last commit is a block commit.
        let current_finalized_commit = self.raw.locate_branch(FINALIZED_BRANCH_NAME.into()).await?;
        let new_commits =
            utils::read_commits(self, current_finalized_commit, block_commit_hash).await?;
        let last_block_header =
            if let Commit::Block(last_block_header) = &new_commits.last().unwrap().0 {
                last_block_header
            } else {
                return Err(eyre!("the last commit is not a block commit"));
            };

        // Check if the given block commit is a descendant of the current finalized branch

        let find_merge_base_result = self
            .raw
            .find_merge_base(current_finalized_commit, block_commit_hash)
            .await
            .map_err(|e| match e {
                raw::Error::NotFound(_) => {
                    eyre!(IntegrityError::new(format!(
                        "cannot find merge base for branch {block_branch_name} and finalized branch"
                    )))
                }
                _ => eyre!(e),
            })?;
        if current_finalized_commit != find_merge_base_result {
            return Err(eyre!(
                "block commit is not a descendant of the current finalized branch"
            ));
        }

        // Verify every commit along the way.
        let last_finalized_block_header = self.get_last_finalized_block_header().await?;
        let reserved_state = self.get_reserved_state().await?;
        let mut verifier = CommitSequenceVerifier::new(
            last_finalized_block_header.clone(),
            reserved_state.clone(),
        )
        .map_err(|e| eyre!("failed to create a commit sequence verifier: {}", e))?;
        for (new_commit, new_commit_hash) in &new_commits {
            verifier
                .apply_commit(new_commit)
                .map_err(|e| eyre!("verification error on commit {}: {}", new_commit_hash, e))?;
        }
        verifier
            .verify_last_header_finalization(last_block_proof)
            .map_err(|e| eyre!("verification error on the last block header: {}", e))?;

        // If commit sequence verification is done and the finalization proof is verified,
        // move the `finalized` branch to the given block commit hash.
        // Then we update the `fp` branch.
        self.raw.checkout_clean().await?;
        self.raw
            .move_branch(FINALIZED_BRANCH_NAME.to_string(), block_commit_hash)
            .await?;
        self.raw
            .move_branch(FP_BRANCH_NAME.to_string(), block_commit_hash)
            .await?;
        self.raw
            .checkout(FP_BRANCH_NAME.into())
            .await
            .map_err(|e| match e {
                raw::Error::NotFound(_) => {
                    eyre!(IntegrityError::new(format!(
                        "failed to checkout to the fp branch: {e}"
                    )))
                }
                _ => eyre!(e),
            })?;
        self.raw
            .create_semantic_commit(format::fp_to_semantic_commit(&LastFinalizationProof {
                height: last_block_header.height,
                proof: last_block_proof.clone(),
            }))
            .await?;
        Ok(())
    }

    /// Returns the currently valid and height-acceptable agendas in the repository.
    pub async fn get_agendas(&self) -> Result<Vec<(CommitHash, Hash256)>, Error> {
        let mut agendas: Vec<(CommitHash, Hash256)> = vec![];
        let branches = retrieve_local_branches(&self.raw).await?;
        let last_header_commit_hash = self.raw.locate_branch(FINALIZED_BRANCH_NAME.into()).await?;
        for (branch, branch_commit_hash) in branches {
            // Check if the branch is an agenda branch
            if branch.as_str().starts_with("a-") {
                // Check if the agenda branch is rebased on top of the `finalized` branch
                let find_merge_base_result = self
                    .raw
                    .find_merge_base(last_header_commit_hash, branch_commit_hash)
                    .await
                    .map_err(|e| match e {
                        raw::Error::NotFound(_) => {
                            eyre!(IntegrityError::new(format!(
                                "cannot find merge base for branch {branch} and finalized branch"
                            )))
                        }
                        _ => eyre!(e),
                    })?;

                if last_header_commit_hash != find_merge_base_result {
                    log::warn!(
                        "branch {} should be rebased on top of the {} branch",
                        branch,
                        FINALIZED_BRANCH_NAME
                    );
                    continue;
                }

                // Push currently valid and height-acceptable agendas to the list
                let commits =
                    read_commits(self, last_header_commit_hash, branch_commit_hash).await?;
                let last_header = self.get_last_finalized_block_header().await?;
                for (commit, hash) in commits {
                    if let Commit::Agenda(agenda) = commit {
                        if agenda.height == last_header.height + 1 {
                            agendas.push((hash, agenda.to_hash256()));
                        }
                    }
                }
            }
        }
        Ok(agendas)
    }

    /// Returns the currently valid and height-acceptable blocks in the repository.
    pub async fn get_blocks(&self) -> Result<Vec<(CommitHash, Hash256)>, Error> {
        let mut blocks: Vec<(CommitHash, Hash256)> = vec![];
        let branches = retrieve_local_branches(&self.raw).await?;
        let last_header_commit_hash = self.raw.locate_branch(FINALIZED_BRANCH_NAME.into()).await?;
        for (branch, branch_commit_hash) in branches {
            // Check if the branch is a block branch
            if branch.as_str().starts_with("b-") {
                // Check if the block branch is rebased on top of the `finalized` branch
                let find_merge_base_result = self
                    .raw
                    .find_merge_base(last_header_commit_hash, branch_commit_hash)
                    .await
                    .map_err(|e| match e {
                        raw::Error::NotFound(_) => {
                            eyre!(IntegrityError::new(format!(
                                "cannot find merge base for branch {branch} and finalized branch"
                            )))
                        }
                        _ => eyre!(e),
                    })?;
                if last_header_commit_hash != find_merge_base_result {
                    log::warn!(
                        "branch {} should be rebased on top of the {} branch",
                        branch,
                        FINALIZED_BRANCH_NAME
                    );
                    continue;
                }

                // Push currently valid and height-acceptable blocks to the list
                let commits =
                    read_commits(self, last_header_commit_hash, branch_commit_hash).await?;
                let last_header = self.get_last_finalized_block_header().await?;
                for (commit, hash) in commits {
                    if let Commit::Block(block_header) = commit {
                        if block_header.height == last_header.height + 1 {
                            blocks.push((hash, block_header.to_hash256()));
                        }
                    }
                }
            }
        }
        Ok(blocks)
    }

    /// Informs that the given agenda has been approved.
    ///
    ///
    /// After verification, it will create an agenda-proof commit,
    /// and update the corresponding `a-#` branch to it
    pub async fn approve(
        &mut self,
        agenda_hash: &Hash256,
        proof: Vec<TypedSignature<Agenda>>,
        timestamp: Timestamp,
    ) -> Result<CommitHash, Error> {
        // Check if the agenda branch is rebased on top of the `finalized` branch.
        let last_header_commit = self.raw.locate_branch(FINALIZED_BRANCH_NAME.into()).await?;
        let agenda_branch_name =
            format!("a-{}", &agenda_hash.to_string()[0..BRANCH_NAME_HASH_DIGITS]);
        let agenda_commit_hash = self.raw.locate_branch(agenda_branch_name.clone()).await?;
        let find_merge_base_result = self
            .raw
            .find_merge_base(last_header_commit, agenda_commit_hash)
            .await
            .map_err(|e| match e {
                raw::Error::NotFound(_) => {
                    eyre!(IntegrityError::new(format!(
                        "cannot find merge base for branch {agenda_branch_name} and finalized branch"
                    )))
                }
                _ => eyre!(e),
            })?;

        if last_header_commit != find_merge_base_result {
            return Err(eyre!(
                "branch {} should be rebased on {}",
                agenda_branch_name,
                FINALIZED_BRANCH_NAME
            ));
        }

        // Verify all the incoming commits
        let finalized_header = self.get_last_finalized_block_header().await?;
        let reserved_state = self.get_reserved_state().await?;
        let finalized_commit_hash = self.raw.locate_branch(FINALIZED_BRANCH_NAME.into()).await?;
        let commits = utils::read_commits(self, finalized_commit_hash, agenda_commit_hash).await?;
        let mut verifier =
            CommitSequenceVerifier::new(finalized_header.clone(), reserved_state.clone())
                .map_err(|e| eyre!("failed to create a commit sequence verifier: {}", e))?;
        for (commit, hash) in commits.iter() {
            verifier
                .apply_commit(commit)
                .map_err(|e| eyre!("verification error on commit {}: {}", hash, e))?;
        }
        // Verify agenda with agenda proof
        let agenda_commit = commits.iter().map(|(commit, _)| commit).last().unwrap();
        let agenda = match agenda_commit {
            Commit::Agenda(agenda) => agenda,
            _ => return Err(eyre::eyre!("not an agenda commit")),
        };
        // Delete past `a-(trimmed agenda hash)` branch and create new `a-(trimmed agenda proof hash)` branch
        self.raw.delete_branch(agenda_branch_name.clone()).await?;
        // Create agenda proof commit
        let agenda_proof = AgendaProof {
            height: agenda.height,
            agenda_hash: agenda_commit.to_hash256(),
            proof,
            timestamp,
        };

        let agenda_proof_commit = Commit::AgendaProof(agenda_proof.clone());
        let agenda_proof_semantic_commit =
            format::to_semantic_commit(&agenda_proof_commit, reserved_state)?;
        let agenda_proof_branch_name = format!(
            "a-{}",
            &agenda_proof_commit.to_hash256().to_string()[0..BRANCH_NAME_HASH_DIGITS]
        );
        // Check if it is already approved.
        if self
            .raw
            .list_branches()
            .await?
            .contains(&agenda_proof_branch_name)
        {
            return Ok(self
                .raw
                .locate_branch(agenda_proof_branch_name.clone())
                .await?);
        }
        self.raw
            .create_branch(agenda_proof_branch_name.clone(), agenda_commit_hash)
            .await?;
        self.raw.checkout(agenda_proof_branch_name).await?;
        let agenda_proof_commit_hash = self
            .raw
            .create_semantic_commit(agenda_proof_semantic_commit)
            .await?;

        Ok(agenda_proof_commit_hash)
    }

    /// Creates an agenda commit on top of the `work` branch.
    pub async fn create_agenda(
        &mut self,
        author: MemberName,
    ) -> Result<(Agenda, CommitHash), Error> {
        let last_header = self.get_last_finalized_block_header().await?;
        let work_commit = self.raw.locate_branch(WORK_BRANCH_NAME.into()).await?;
        let last_header_commit = self.raw.locate_branch(FINALIZED_BRANCH_NAME.into()).await?;

        // Check if the `work` branch is rebased on top of the `finalized` branch.
        if self
            .raw
            .find_merge_base(last_header_commit, work_commit)
            .await?
            != last_header_commit
        {
            return Err(eyre!(
                "branch {} should be rebased on {}",
                WORK_BRANCH_NAME,
                FINALIZED_BRANCH_NAME
            ));
        }
        // Check the validity of the commit sequence
        let reserved_state = self.get_reserved_state().await?;
        let mut verifier = CommitSequenceVerifier::new(last_header.clone(), reserved_state.clone())
            .map_err(|e| eyre!("failed to create a commit sequence verifier: {}", e))?;
        let commits = read_commits(self, last_header_commit, work_commit).await?;
        for (commit, hash) in commits.iter() {
            verifier
                .apply_commit(commit)
                .map_err(|e| eyre!("verification error on commit {}: {}", hash, e))?;
        }

        // Create agenda commit
        let mut transactions = Vec::new();
        for (commit, _) in commits {
            if let Commit::Transaction(t) = commit {
                transactions.push(t.clone());
            }
        }
        let agenda = Agenda {
            author,
            timestamp: get_timestamp(),
            transactions_hash: Agenda::calculate_transactions_hash(&transactions),
            height: last_header.height + 1,
        };
        let agenda_commit = Commit::Agenda(agenda.clone());
        verifier.apply_commit(&agenda_commit).map_err(|_| {
            eyre!("agenda commit cannot be created on top of the current commit sequence")
        })?;

        let semantic_commit = to_semantic_commit(&agenda_commit, reserved_state)?;

        self.raw.checkout_clean().await?;
        self.raw
            .checkout(WORK_BRANCH_NAME.into())
            .await
            .map_err(|e| match e {
                raw::Error::NotFound(_) => {
                    eyre!(IntegrityError::new(format!(
                        "failed to checkout to the work branch: {e}"
                    )))
                }
                _ => eyre!(e),
            })?;
        let result = self.raw.create_semantic_commit(semantic_commit).await?;
        let mut agenda_branch_name = agenda_commit.to_hash256().to_string();
        agenda_branch_name.truncate(BRANCH_NAME_HASH_DIGITS);
        let agenda_branch_name = format!("a-{agenda_branch_name}");
        self.raw.create_branch(agenda_branch_name, result).await?;
        Ok((agenda, result))
    }

    /// Puts a 'vote' tag on the commit.
    pub async fn vote(&mut self, commit_hash: CommitHash) -> Result<(), Error> {
        let semantic_commit = self.raw.read_semantic_commit(commit_hash).await?;
        let commit = format::from_semantic_commit(semantic_commit).map_err(|e| eyre!(e))?;
        // Check if the commit is an agenda commit.
        if let Commit::Agenda(_) = commit {
            let mut vote_tag_name = commit.to_hash256().to_string();
            vote_tag_name.truncate(TAG_NAME_HASH_DIGITS);
            let vote_tag_name = format!("vote-{vote_tag_name}");
            self.raw.create_tag(vote_tag_name, commit_hash).await?;
            Ok(())
        } else {
            Err(eyre!("commit {} is not an agenda commit", commit_hash))
        }
    }

    /// Puts a 'veto' tag on the commit.
    pub async fn veto(&mut self, commit_hash: CommitHash) -> Result<(), Error> {
        let semantic_commit = self.raw.read_semantic_commit(commit_hash).await?;
        let commit = format::from_semantic_commit(semantic_commit).map_err(|e| eyre!(e))?;
        // Check if the commit is a block commit.
        if let Commit::Block(_) = commit {
            let mut veto_tag_name = commit.to_hash256().to_string();
            veto_tag_name.truncate(TAG_NAME_HASH_DIGITS);
            let veto_tag_name = format!("veto-{veto_tag_name}");
            self.raw.create_tag(veto_tag_name, commit_hash).await?;
            Ok(())
        } else {
            Err(eyre!("commit {} is not a block commit", commit_hash))
        }
    }

    /// Creates a block commit on top of the `work` branch.
    pub async fn create_block(
        &mut self,
        author: PublicKey,
    ) -> Result<(BlockHeader, CommitHash), Error> {
        let work_commit = self.raw.locate_branch(WORK_BRANCH_NAME.into()).await?;
        let last_header_commit = self.raw.locate_branch(FINALIZED_BRANCH_NAME.into()).await?;

        // Check if the `work` branch is rebased on top of the `finalized` branch.
        if self
            .raw
            .find_merge_base(last_header_commit, work_commit)
            .await?
            != last_header_commit
        {
            return Err(eyre!(
                "branch {} should be rebased on {}",
                WORK_BRANCH_NAME,
                FINALIZED_BRANCH_NAME
            ));
        }

        // Check the validity of the commit sequence
        let commits = read_commits(self, last_header_commit, work_commit).await?;
        let last_header = self.get_last_finalized_block_header().await?;
        self.raw
            .checkout(WORK_BRANCH_NAME.into())
            .await
            .map_err(|e| match e {
                raw::Error::NotFound(_) => {
                    eyre!(IntegrityError::new(format!(
                        "failed to checkout to the work branch: {e}"
                    )))
                }
                _ => eyre!(e),
            })?;
        let reserved_state = self.get_reserved_state().await?;
        let mut verifier = CommitSequenceVerifier::new(last_header.clone(), reserved_state.clone())
            .map_err(|e| eyre!("failed to create a commit sequence verifier: {}", e))?;
        for (commit, hash) in commits.iter() {
            verifier
                .apply_commit(commit)
                .map_err(|e| eyre!("verification error on commit {}: {}", hash, e))?;
        }

        // Verify `finalization_proof`
        let fp_commit_hash = self.raw.locate_branch(FP_BRANCH_NAME.into()).await?;
        let fp_semantic_commit = self.raw.read_semantic_commit(fp_commit_hash).await?;
        let finalization_proof = fp_from_semantic_commit(fp_semantic_commit).unwrap().proof;

        // Create block commit
        let block_header = BlockHeader {
            author: author.clone(),
            prev_block_finalization_proof: finalization_proof,
            previous_hash: last_header.to_hash256(),
            height: last_header.height + 1,
            timestamp: get_timestamp(),
            commit_merkle_root: BlockHeader::calculate_commit_merkle_root(
                &commits
                    .iter()
                    .map(|(commit, _)| commit.clone())
                    .collect::<Vec<_>>(),
            ),
            repository_merkle_root: Hash256::zero(), // TODO
            validator_set: reserved_state.get_validator_set().unwrap(),
            version: SIMPERBY_CORE_PROTOCOL_VERSION.to_string(),
        };
        let block_commit = Commit::Block(block_header.clone());
        verifier.apply_commit(&block_commit).map_err(|_| {
            eyre!("block commit cannot be created on top of the current commit sequence")
        })?;

        let semantic_commit = to_semantic_commit(&block_commit, reserved_state)?;

        self.raw.checkout_clean().await?;
        self.raw
            .checkout(WORK_BRANCH_NAME.into())
            .await
            .map_err(|e| match e {
                raw::Error::NotFound(_) => {
                    eyre!(IntegrityError::new(format!(
                        "failed to checkout to the work branch: {e}"
                    )))
                }
                _ => eyre!(e),
            })?;
        let result = self.raw.create_semantic_commit(semantic_commit).await?;
        let mut block_branch_name = block_commit.to_hash256().to_string();
        block_branch_name.truncate(BRANCH_NAME_HASH_DIGITS);
        let block_branch_name = format!("b-{block_branch_name}");
        self.raw.create_branch(block_branch_name, result).await?;
        Ok((block_header, result))
    }

    /// Creates an extra-agenda transaction commit on top of the `work` branch.
    pub async fn create_extra_agenda_transaction(
        &mut self,
        transaction: &ExtraAgendaTransaction,
    ) -> Result<CommitHash, Error> {
        let work_commit = self.raw.locate_branch(WORK_BRANCH_NAME.into()).await?;
        let last_header_commit = self.raw.locate_branch(FINALIZED_BRANCH_NAME.into()).await?;
        let reserved_state = self.get_reserved_state().await?;

        // Check if the `work` branch is rebased on top of the `finalized` branch.
        if self
            .raw
            .find_merge_base(last_header_commit, work_commit)
            .await?
            != last_header_commit
        {
            return Err(eyre!(
                "branch {} should be rebased on {}",
                WORK_BRANCH_NAME,
                FINALIZED_BRANCH_NAME
            ));
        }

        // Check the validity of the commit sequence
        let commits = read_commits(self, last_header_commit, work_commit).await?;
        let last_header = self.get_last_finalized_block_header().await?;
        let mut verifier = CommitSequenceVerifier::new(last_header.clone(), reserved_state.clone())
            .map_err(|e| eyre!("failed to create a commit sequence verifier: {}", e))?;
        for (commit, hash) in commits.iter() {
            verifier
                .apply_commit(commit)
                .map_err(|e| eyre!("verification error on commit {}: {}", hash, e))?;
        }

        let extra_agenda_tx_commit = Commit::ExtraAgendaTransaction(transaction.clone());
        verifier.apply_commit(&extra_agenda_tx_commit).map_err(|_| {
            eyre!(
                "extra-agenda transaction commit cannot be created on top of the current commit sequence"
            )
        })?;

        let semantic_commit = to_semantic_commit(&extra_agenda_tx_commit, reserved_state)?;

        self.raw.checkout_clean().await?;
        self.raw.checkout(WORK_BRANCH_NAME.into()).await?;
        let result = self.raw.create_semantic_commit(semantic_commit).await?;
        Ok(result)
    }
}
