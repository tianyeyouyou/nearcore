use std::time::Instant;

use near_chain::types::{RuntimeStorageConfig, StorageDataSource};
use near_chain::{Block, BlockHeader};
use near_chain_primitives::Error;
use near_primitives::sharding::{ShardChunk, ShardChunkHeader};
use near_primitives::stateless_validation::EncodedChunkStateWitness;

use crate::stateless_validation::chunk_validator::{
    pre_validate_chunk_state_witness, validate_chunk_state_witness, validate_prepared_transactions,
    MainStateTransitionCache,
};
use crate::{metrics, Client};

impl Client {
    // Temporary feature to make node produce state witness for every chunk in every processed block
    // and then self-validate it.
    pub(crate) fn shadow_validate_block_chunks(&mut self, block: &Block) -> Result<(), Error> {
        if !cfg!(feature = "shadow_chunk_validation") {
            return Ok(());
        }
        let block_hash = block.hash();
        tracing::debug!(target: "client", ?block_hash, "shadow validation for block chunks");
        let prev_block = self.chain.get_block(block.header().prev_hash())?;
        let prev_block_chunks = prev_block.chunks();
        for chunk in
            block.chunks().iter().filter(|chunk| chunk.is_new_chunk(block.header().height()))
        {
            let chunk = self.chain.get_chunk_clone_from_header(chunk)?;
            let prev_chunk_header = prev_block_chunks.get(chunk.shard_id() as usize).unwrap();
            if let Err(err) =
                self.shadow_validate_chunk(prev_block.header(), prev_chunk_header, &chunk)
            {
                metrics::SHADOW_CHUNK_VALIDATION_FAILED_TOTAL.inc();
                tracing::error!(
                    target: "client",
                    ?err,
                    shard_id = chunk.shard_id(),
                    ?block_hash,
                    "shadow chunk validation failed"
                );
            }
        }
        Ok(())
    }

    fn shadow_validate_chunk(
        &mut self,
        prev_block_header: &BlockHeader,
        prev_chunk_header: &ShardChunkHeader,
        chunk: &ShardChunk,
    ) -> Result<(), Error> {
        let shard_id = chunk.shard_id();
        let chunk_hash = chunk.chunk_hash();
        let chunk_header = chunk.cloned_header();
        let last_chunk = self.chain.get_chunk(&prev_chunk_header.chunk_hash())?;

        let transactions_validation_storage_config = RuntimeStorageConfig {
            state_root: chunk_header.prev_state_root(),
            use_flat_storage: true,
            source: StorageDataSource::Db,
            state_patch: Default::default(),
        };

        // We call `validate_prepared_transactions()` here because we need storage proof for transactions validation.
        // Normally it is provided by chunk producer, but for shadow validation we need to generate it ourselves.
        let Ok(validated_transactions) = validate_prepared_transactions(
            &self.chain,
            self.runtime_adapter.as_ref(),
            &chunk_header,
            transactions_validation_storage_config,
            chunk.transactions(),
            last_chunk.transactions(),
        ) else {
            return Err(Error::Other(
                "Could not produce storage proof for new transactions".to_owned(),
            ));
        };

        let witness = self.create_state_witness(
            // Setting arbitrary chunk producer is OK for shadow validation
            "alice.near".parse().unwrap(),
            prev_block_header,
            prev_chunk_header,
            chunk,
            validated_transactions.storage_proof,
        )?;
        if self.config.save_latest_witnesses {
            self.chain.chain_store.save_latest_chunk_state_witness(&witness)?;
        }
        let (encoded_witness, raw_witness_size) = {
            let shard_id_label = shard_id.to_string();
            let encode_timer = metrics::CHUNK_STATE_WITNESS_ENCODE_TIME
                .with_label_values(&[shard_id_label.as_str()])
                .start_timer();
            let (encoded_witness, raw_witness_size) = EncodedChunkStateWitness::encode(&witness)?;
            encode_timer.observe_duration();
            metrics::record_witness_size_metrics(
                raw_witness_size,
                encoded_witness.size_bytes(),
                &witness,
            );
            let decode_timer = metrics::CHUNK_STATE_WITNESS_DECODE_TIME
                .with_label_values(&[shard_id_label.as_str()])
                .start_timer();
            encoded_witness.decode()?;
            decode_timer.observe_duration();
            (encoded_witness, raw_witness_size)
        };
        let pre_validation_start = Instant::now();
        let pre_validation_result = pre_validate_chunk_state_witness(
            &witness,
            &self.chain,
            self.epoch_manager.as_ref(),
            self.runtime_adapter.as_ref(),
        )?;
        tracing::debug!(
            target: "client",
            shard_id,
            ?chunk_hash,
            witness_size = encoded_witness.size_bytes(),
            raw_witness_size,
            pre_validation_elapsed = ?pre_validation_start.elapsed(),
            "completed shadow chunk pre-validation"
        );
        let epoch_manager = self.epoch_manager.clone();
        let runtime_adapter = self.runtime_adapter.clone();
        rayon::spawn(move || {
            let validation_start = Instant::now();
            match validate_chunk_state_witness(
                witness,
                pre_validation_result,
                epoch_manager.as_ref(),
                runtime_adapter.as_ref(),
                &MainStateTransitionCache::default(),
            ) {
                Ok(()) => {
                    tracing::debug!(
                        target: "client",
                        shard_id,
                        ?chunk_hash,
                        validation_elapsed = ?validation_start.elapsed(),
                        "completed shadow chunk validation"
                    );
                }
                Err(err) => {
                    metrics::SHADOW_CHUNK_VALIDATION_FAILED_TOTAL.inc();
                    tracing::error!(
                        target: "client",
                        ?err,
                        shard_id,
                        ?chunk_hash,
                        "shadow chunk validation failed"
                    );
                }
            }
        });
        Ok(())
    }
}
