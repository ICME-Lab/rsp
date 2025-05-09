use std::{collections::BTreeSet, sync::Arc};

use alloy_consensus::{BlockHeader, Header, Transaction, TxReceipt};
use alloy_evm::EthEvmFactory;
use alloy_primitives::{Bloom, Sealable};
use alloy_provider::{Network, Provider};
use reth_chainspec::ChainSpec;
use reth_evm::{
    execute::{BasicBlockExecutor, Executor},
    ConfigureEvm,
};
use reth_evm_ethereum::EthEvmConfig;
use reth_execution_types::ExecutionOutcome;
use reth_optimism_evm::OpEvmConfig;
use reth_primitives_traits::{Block, BlockBody};
use reth_trie::KeccakKeyHasher;
use revm::database::CacheDB;
use revm_primitives::{Address, B256};
use rsp_client_executor::{
    custom::CustomEvmFactory, io::ClientExecutorInput, IntoInput, IntoPrimitives,
    ValidateBlockPostExecution,
};
use rsp_mpt::EthereumState;
use rsp_primitives::{account_proof::eip1186_proof_to_account_proof, genesis::Genesis};
use rsp_rpc_db::RpcDb;

use crate::HostError;

pub type EthHostExecutor = HostExecutor<EthEvmConfig<CustomEvmFactory<EthEvmFactory>>>;

pub type OpHostExecutor = HostExecutor<OpEvmConfig>;

/// An executor that fetches data from a [Provider] to execute blocks in the [ClientExecutor].
#[derive(Debug, Clone)]
pub struct HostExecutor<C: ConfigureEvm> {
    evm_config: C,
}

impl EthHostExecutor {
    pub fn eth(chain_spec: Arc<ChainSpec>, custom_beneficiary: Option<Address>) -> Self {
        Self {
            evm_config: EthEvmConfig::new_with_evm_factory(
                chain_spec,
                CustomEvmFactory::<EthEvmFactory>::new(custom_beneficiary),
            ),
        }
    }
}

impl OpHostExecutor {
    pub fn optimism(chain_spec: Arc<reth_optimism_chainspec::OpChainSpec>) -> Self {
        Self { evm_config: OpEvmConfig::optimism(chain_spec) }
    }
}

impl<C: ConfigureEvm> HostExecutor<C> {
    /// Creates a new [HostExecutor].
    pub fn new(evm_config: C) -> Self {
        Self { evm_config }
    }

    /// Executes the block with the given block number.
    pub async fn execute<P, N>(
        &self,
        block_number: u64,
        rpc_db: &RpcDb<P, N>,
        provider: &P,
        genesis: Genesis,
        custom_beneficiary: Option<Address>,
        opcode_tracking: bool,
    ) -> Result<ClientExecutorInput<C::Primitives>, HostError>
    where
        C::Primitives: IntoPrimitives<N> + IntoInput + ValidateBlockPostExecution,
        P: Provider<N> + Clone,
        N: Network,
    {
        // Fetch the current block and the previous block from the provider.
        tracing::info!("fetching the current block and the previous block");
        let current_block = provider
            .get_block_by_number(block_number.into())
            .full()
            .await?
            .ok_or(HostError::ExpectedBlock(block_number))
            .map(C::Primitives::into_primitive_block)?;

        let previous_block = provider
            .get_block_by_number((block_number - 1).into())
            .full()
            .await?
            .ok_or(HostError::ExpectedBlock(block_number))
            .map(C::Primitives::into_primitive_block)?;

        // Setup the database for the block executor.
        tracing::info!("setting up the database for the block executor");
        let cache_db = CacheDB::new(rpc_db);

        let block_executor = BasicBlockExecutor::new(self.evm_config.clone(), cache_db);

        // Execute the block and fetch all the necessary data along the way.
        tracing::info!(
            "executing the block with rpc db: block_number={}, transaction_count={}",
            block_number,
            current_block.body().transactions().len()
        );

        // for tx in current_block.body().transactions() {
        //     tracing::info!(
        //         "{:?}: gas price {:?}, max fee {:?}, effective {:?}",
        //         tx.to(),
        //         tx.gas_price(),
        //         tx.max_fee_per_gas(),
        //         tx.effective_gas_price(None)
        //     );
        // }

        let block = current_block
            .clone()
            .try_into_recovered()
            .map_err(|_| HostError::FailedToRecoverSenders)
            .unwrap();

        let execution_output = block_executor.execute(&block)?;

        // Validate the block post execution.
        tracing::info!("validating the block post execution");
        C::Primitives::validate_block_post_execution(&block, &genesis, &execution_output)?;

        // Accumulate the logs bloom.
        tracing::info!("accumulating the logs bloom");
        let mut logs_bloom = Bloom::default();
        execution_output.result.receipts.iter().for_each(|r| {
            logs_bloom.accrue_bloom(&r.bloom());
        });

        // Convert the output to an execution outcome.
        let mut executor_outcome = ExecutionOutcome::new(
            execution_output.state,
            vec![execution_output.result.receipts],
            current_block.header().number(),
            vec![execution_output.result.requests],
        );

        let state_requests = rpc_db.get_state_requests();

        // For every account we touched, fetch the storage proofs for all the slots we touched.
        tracing::info!("fetching storage proofs");
        let mut before_storage_proofs = Vec::new();
        let mut after_storage_proofs = Vec::new();

        for (address, used_keys) in state_requests.iter() {
            let modified_keys = executor_outcome
                .state()
                .state
                .get(address)
                .map(|account| {
                    account.storage.keys().map(|key| B256::from(*key)).collect::<BTreeSet<_>>()
                })
                .unwrap_or_default()
                .into_iter()
                .collect::<Vec<_>>();

            let keys = used_keys
                .iter()
                .map(|key| B256::from(*key))
                .chain(modified_keys.clone().into_iter())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();

            let storage_proof = provider
                .get_proof(*address, keys.clone())
                .block_id((block_number - 1).into())
                .await?;
            before_storage_proofs.push(eip1186_proof_to_account_proof(storage_proof));

            let storage_proof =
                provider.get_proof(*address, modified_keys).block_id((block_number).into()).await?;
            after_storage_proofs.push(eip1186_proof_to_account_proof(storage_proof));
        }

        let state = EthereumState::from_transition_proofs(
            previous_block.header().state_root(),
            &before_storage_proofs.iter().map(|item| (item.address, item.clone())).collect(),
            &after_storage_proofs.iter().map(|item| (item.address, item.clone())).collect(),
        )?;

        // Verify the state root.
        tracing::info!("verifying the state root = {}", executor_outcome.state().state.len());

        // panic!("heree");
        let state_root =
            {
                let mut mutated_state = state.clone();
                executor_outcome.bundle.state.retain(|key, _| {
                key == &alloy_primitives::address!("0x000000629fbcf27a347d1aeba658435230d74a5f") ||
key == &alloy_primitives::address!("0x037dd48ffd09fbdc1e385fefda48c6e1ef1382af") ||
key == &alloy_primitives::address!("0x06a9ab27c7e2255df1815e6cc0168d7755feb19a") ||
key == &alloy_primitives::address!("0x08e96f308eb008b3db68640aba6b06078625f8cd") ||
key == &alloy_primitives::address!("0x0d0707963952f2fba59dd06f2b425ace40b492fe") ||
key == &alloy_primitives::address!("0x111111125421ca6dc452d289314280a0f8842a65") ||
key == &alloy_primitives::address!("0x12106758e03613e66fa96209927940c825e85fff") ||
key == &alloy_primitives::address!("0x1516008376543c283654f60b03a28e1c9930806a") ||
key == &alloy_primitives::address!("0x16c0829dd60124f2a7d49a5e768f7978a57c2393") ||
key == &alloy_primitives::address!("0x1728d7099f6535f5efeba784a4ba54120ceada6b") ||
key == &alloy_primitives::address!("0x1d71eb5d4f05884add4d8e8a4d31eef3a4263c47") ||
key == &alloy_primitives::address!("0x23529b46bb5fdb9f9d0427e9a35115551b72581b") ||
key == &alloy_primitives::address!("0x239426c2feda17d10635b6e7d1cfca9ab33ab222") ||
key == &alloy_primitives::address!("0x26c1087b6a658c106768eea1931e083ce469f20c") ||
key == &alloy_primitives::address!("0x30daff27da012e118c07fae5380eb06f707c5ce4") ||
key == &alloy_primitives::address!("0x340d2bde5eb28c1eed91b2f790723e3b160613b7") ||
key == &alloy_primitives::address!("0x3777261fd6e1ec0704735d491328215b9f5825b1") ||
key == &alloy_primitives::address!("0x4280b10e7cd12171e944401e4018250d2052a0d6") ||
key == &alloy_primitives::address!("0x4a5565db6515923418bb9ab1a8ad816e85c12ff4") ||
key == &alloy_primitives::address!("0x4cff49d0a19ed6ff845a9122fa912abcfb1f68a6") ||
key == &alloy_primitives::address!("0x4d224452801aced8b2f0aebe155379bb5d594381") ||
key == &alloy_primitives::address!("0x4d9ff50ef4da947364bb9650892b2554e7be5e2b") ||
key == &alloy_primitives::address!("0x5c9538085fdfce7470e66f7c3e1b1f0f01d969aa") ||
key == &alloy_primitives::address!("0x5faa989af96af85384b8a938c2ede4a7378d9875") ||
key == &alloy_primitives::address!("0x671e1c289f45ccaa82843501c7bc841ba26b97f1")

// key == &alloy_primitives::address!("0x6887246668a3b87f54deb3b94ba47a6f63f32985")
// key == &alloy_primitives::address!("0x6c5146e923ce3854ed3cf73aafee10fda770e92b") ||
// key == &alloy_primitives::address!("0x6cc5f688a315f3dc28a7781717a9a798a59fda7b") ||
// key == &alloy_primitives::address!("0x6f7977ad0d71e89a70e70816dd7a04928c9ece99") ||
// key == &alloy_primitives::address!("0x7039cd6d7966672f194e8139074c3d5c4e6dcf65") ||
// key == &alloy_primitives::address!("0x71306dbdcd14b1770ceec15de46bb9e9c1f61022") ||
// key == &alloy_primitives::address!("0x71439c54126bfd73d6757b3f1b0cb1b74a7be3a7")
            });
                tracing::info!("{:#?}", executor_outcome.bundle.state);

                tracing::info!(
                    "state for 0x6887246668a3b87f54deb3b94ba47a6f63f32985: {:#?}",
                    executor_outcome.bundle.state.get(&alloy_primitives::address!(
                        "0x6887246668a3b87f54deb3b94ba47a6f63f32985"
                    ))
                );

                mutated_state.update(&executor_outcome.hash_state_slow::<KeccakKeyHasher>());
                mutated_state.state_root()
            };

        // if state_root != current_block.header().state_root() {
        //     return Err(HostError::StateRootMismatch(
        //         state_root,
        //         current_block.header().state_root(),
        //     ));
        // }

        tracing::info!("state root = {:?}", state_root);
        panic!("zzzz");

        // Derive the block header.
        //
        // Note: the receipts root and gas used are verified by `validate_block_post_execution`.
        let header = Header {
            parent_hash: current_block.header().parent_hash(),
            ommers_hash: current_block.header().ommers_hash(),
            beneficiary: current_block.header().beneficiary(),
            state_root,
            transactions_root: current_block.header().transactions_root(),
            receipts_root: current_block.header().receipts_root(),
            logs_bloom,
            difficulty: current_block.header().difficulty(),
            number: current_block.header().number(),
            gas_limit: current_block.header().gas_limit(),
            gas_used: current_block.header().gas_used(),
            timestamp: current_block.header().timestamp(),
            extra_data: current_block.header().extra_data().clone(),
            mix_hash: current_block.header().mix_hash().unwrap(),
            nonce: current_block.header().nonce().unwrap(),
            base_fee_per_gas: current_block.header().base_fee_per_gas(),
            withdrawals_root: current_block.header().withdrawals_root(),
            blob_gas_used: current_block.header().blob_gas_used(),
            excess_blob_gas: current_block.header().excess_blob_gas(),
            parent_beacon_block_root: current_block.header().parent_beacon_block_root(),
            requests_hash: current_block.header().requests_hash(),
        };

        // Assert the derived header is correct.
        let constructed_header_hash = header.hash_slow();
        let target_hash = current_block.header().hash_slow();
        if constructed_header_hash != target_hash {
            return Err(HostError::HeaderMismatch(constructed_header_hash, target_hash));
        }

        // Log the result.
        tracing::info!(
            "successfully executed block: block_number={}, block_hash={}, state_root={}",
            current_block.header().number(),
            constructed_header_hash,
            state_root
        );

        // Fetch the parent headers needed to constrain the BLOCKHASH opcode.
        let oldest_ancestor = *rpc_db.oldest_ancestor.read().unwrap();
        let mut ancestor_headers = vec![];
        tracing::info!("fetching {} ancestor headers", block_number - oldest_ancestor);
        for height in (oldest_ancestor..=(block_number - 1)).rev() {
            let block = provider
                .get_block_by_number(height.into())
                .await?
                .ok_or(HostError::ExpectedBlock(height))?;

            ancestor_headers.push(C::Primitives::into_primitive_header(block))
        }

        // Create the client input.
        let client_input = ClientExecutorInput {
            current_block: C::Primitives::into_input_block(current_block),
            ancestor_headers,
            parent_state: state,
            state_requests,
            bytecodes: rpc_db.get_bytecodes(),
            genesis,
            custom_beneficiary,
            opcode_tracking,
        };
        tracing::info!("successfully generated client input");

        Ok(client_input)
    }
}
