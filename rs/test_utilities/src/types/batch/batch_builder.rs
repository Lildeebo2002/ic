use std::collections::BTreeMap;

use crate::util::mock_time;
use ic_types::{
    batch::{Batch, BatchMessages, BlockmakerMetrics},
    Height, Randomness, RegistryVersion, Time,
};

pub struct BatchBuilder {
    batch: Batch,
}

impl Default for BatchBuilder {
    /// Create a default, empty, XNetPayload
    fn default() -> Self {
        Self {
            batch: Batch {
                batch_number: Height::from(0),
                requires_full_state_hash: false,
                messages: BatchMessages::default(),
                randomness: Randomness::from([0; 32]),
                ecdsa_subnet_public_keys: BTreeMap::new(),
                registry_version: RegistryVersion::from(1),
                time: mock_time(),
                consensus_responses: vec![],
                blockmaker_metrics: BlockmakerMetrics::new_for_test(),
            },
        }
    }
}

impl BatchBuilder {
    /// Creates a new `BatchBuilder`.
    pub fn new() -> Self {
        Default::default()
    }

    /// Sets the `batch_number` field.
    pub fn batch_number(mut self, batch_number: Height) -> Self {
        self.batch.batch_number = batch_number;
        self
    }

    /// Sets the `messages` field.
    pub fn messages(mut self, messages: BatchMessages) -> Self {
        self.batch.messages = messages;
        self
    }

    /// Sets the `randomness` field.
    pub fn randomness(mut self, randomness: Randomness) -> Self {
        self.batch.randomness = randomness;
        self
    }

    /// Sets the `registry_version` field.
    pub fn registry_version(mut self, registry_version: RegistryVersion) -> Self {
        self.batch.registry_version = registry_version;
        self
    }

    /// Sets the `time` field.
    pub fn time(mut self, time: Time) -> Self {
        self.batch.time = time;
        self
    }

    /// Returns the built `Batch`.
    pub fn build(&self) -> Batch {
        self.batch.clone()
    }
}
