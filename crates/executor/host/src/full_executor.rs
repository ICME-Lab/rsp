#![allow(unused)]

use std::{
    fmt::{Debug, Formatter},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_consensus::{BlockHeader, Transaction};
use alloy_provider::Provider;
use either::Either;
use eyre::bail;
use reth_ethereum_primitives::EthPrimitives;
use reth_primitives_traits::{Block, Header, NodePrimitives, SignedTransaction};
use reth_trie::{HashedPostState, KeccakKeyHasher};
use revm::{context::ContextTr, inspector::JournalExt};
use revm_primitives::B256;
use rsp_client_executor::io::ClientExecutorInput;
use rsp_rpc_db::RpcDb;
use serde::de::DeserializeOwned;
use sp1_prover::components::CpuProverComponents;
use sp1_sdk::{
    ExecutionReport, Prover, SP1ProofMode, SP1ProvingKey, SP1PublicValues, SP1Stdin,
    SP1VerifyingKey,
};
use tokio::{task, time::sleep};
use tracing::{error, info, info_span, warn};

use crate::{Config, ExecutionHooks, ExecutorComponents, HostExecutor};

pub type EitherExecutor<C, P> = Either<FullExecutor<C, P>, CachedExecutor<C>>;

pub async fn build_executor<C, P>(
    elf: Vec<u8>,
    provider: Option<P>,
    evm_config: C::EvmConfig,
    client: Arc<C::Prover>,
    hooks: C::Hooks,
    config: Config,
) -> eyre::Result<EitherExecutor<C, P>>
where
    C: ExecutorComponents,
    P: Provider<C::Network> + Clone,
{
    if let Some(provider) = provider {
        return Ok(Either::Left(
            FullExecutor::try_new(provider, elf, evm_config, client, hooks, config).await?,
        ));
    }

    if let Some(cache_dir) = config.cache_dir {
        return Ok(Either::Right(
            CachedExecutor::try_new(
                elf,
                client,
                hooks,
                cache_dir,
                config.chain.id(),
                config.prove_mode,
            )
            .await?,
        ));
    }

    bail!("Either a RPC URL or a cache dir must be provided")
}

pub trait BlockExecutor<C: ExecutorComponents> {
    #[allow(async_fn_in_trait)]
    async fn execute(&self, block_number: u64) -> eyre::Result<()>;

    fn client(&self) -> Arc<C::Prover>;

    fn pk(&self) -> Arc<SP1ProvingKey>;

    fn vk(&self) -> Arc<SP1VerifyingKey>;

    #[allow(async_fn_in_trait)]
    async fn process_client(
        &self,
        client_input: ClientExecutorInput<C::Primitives>,
        hooks: &C::Hooks,
        prove_mode: Option<SP1ProofMode>,
    ) -> eyre::Result<()> {
        // Generate the proof.
        // Execute the block inside the zkVM.
        let mut stdin = SP1Stdin::new();
        let buffer = bincode::serialize(&client_input).unwrap();

        stdin.write_vec(buffer);

        // Only execute the program.
        let (stdin, execute_result) =
            execute_client(client_input.current_block.number, self.client(), self.pk(), stdin)
                .await?;
        let (mut public_values, execution_report) = execute_result?;

        // Read the block hash.
        let block_hash = public_values.read::<B256>();
        info!(?block_hash, "Execution sucessful");

        hooks
            .on_execution_end::<C::Primitives>(&client_input.current_block, &execution_report)
            .await?;

        if let Some(prove_mode) = prove_mode {
            info!("Starting proof generation");

            let proving_start = Instant::now();
            hooks.on_proving_start(client_input.current_block.number).await?;
            let client = self.client();
            let pk = self.pk();

            let proof = task::spawn_blocking(move || {
                client.prove(pk.as_ref(), &stdin, prove_mode).map_err(|err| eyre::eyre!("{err}"))
            })
            .await
            .map_err(|err| eyre::eyre!("{err}"))??;

            let proving_duration = proving_start.elapsed();
            let proof_bytes = bincode::serialize(&proof.proof).unwrap();

            hooks
                .on_proving_end(
                    client_input.current_block.number,
                    &proof_bytes,
                    self.vk().as_ref(),
                    &execution_report,
                    proving_duration,
                )
                .await?;

            info!("Proof successfully generated!");
        }

        Ok(())
    }
}

impl<C, P> BlockExecutor<C> for EitherExecutor<C, P>
where
    C: ExecutorComponents,
    P: Provider<C::Network> + Clone,
{
    async fn execute(&self, block_number: u64) -> eyre::Result<()> {
        match self {
            Either::Left(ref executor) => executor.execute(block_number).await,
            Either::Right(ref executor) => executor.execute(block_number).await,
        }
    }

    fn client(&self) -> Arc<C::Prover> {
        match self {
            Either::Left(ref executor) => executor.client.clone(),
            Either::Right(ref executor) => executor.client.clone(),
        }
    }

    fn pk(&self) -> Arc<SP1ProvingKey> {
        match self {
            Either::Left(ref executor) => executor.pk.clone(),
            Either::Right(ref executor) => executor.pk.clone(),
        }
    }

    fn vk(&self) -> Arc<SP1VerifyingKey> {
        match self {
            Either::Left(ref executor) => executor.vk.clone(),
            Either::Right(ref executor) => executor.vk.clone(),
        }
    }
}

pub struct FullExecutor<C, P>
where
    C: ExecutorComponents,
    P: Provider<C::Network> + Clone,
{
    provider: P,
    host_executor: HostExecutor<C::EvmConfig>,
    client: Arc<C::Prover>,
    pk: Arc<SP1ProvingKey>,
    vk: Arc<SP1VerifyingKey>,
    hooks: C::Hooks,
    config: Config,
}

impl<C, P> FullExecutor<C, P>
where
    C: ExecutorComponents,
    P: Provider<C::Network> + Clone,
{
    pub async fn try_new(
        provider: P,
        elf: Vec<u8>,
        evm_config: C::EvmConfig,
        client: Arc<C::Prover>,
        hooks: C::Hooks,
        config: Config,
    ) -> eyre::Result<Self> {
        let cloned_client = client.clone();

        // Setup the proving key and verification key.
        let (pk, vk) = task::spawn_blocking(move || {
            let (pk, vk) = cloned_client.setup(&elf);
            (pk, vk)
        })
        .await?;

        Ok(Self {
            provider,
            host_executor: HostExecutor::new(evm_config),
            client,
            pk: Arc::new(pk),
            vk: Arc::new(vk),
            hooks,
            config,
        })
    }

    pub async fn wait_for_block(&self, block_number: u64) -> eyre::Result<()> {
        let block_number = block_number.into();

        while self.provider.get_block_by_number(block_number).await?.is_none() {
            sleep(Duration::from_millis(100)).await;
        }
        Ok(())
    }

    pub async fn test(&self) {
        use revm::{
            database::{CacheDB, StateBuilder, WrapDatabaseRef},
            inspector::{InspectEvm, NoOpInspector},
            primitives::TxKind,
            Context, ExecuteCommitEvm, ExecuteEvm, MainBuilder, MainContext,
        };

        info!("trying to read file from disk");
        let input = tokio::fs::read("/home/altonen/work/rsp/20526624.bin").await.unwrap();

        info!("trying to deserialize input");
        let mut input = bincode::deserialize::<ClientExecutorInput<EthPrimitives>>(&input).unwrap();

        info!("trying to create witness db");
        let trie_db = input.witness_db().unwrap();
        let db = WrapDatabaseRef(trie_db);

        info!("trying to create block");
        let block = input.current_block.clone();

        info!("create context");
        let mut test =
            revm::database::StateBuilder::new_with_database(db).with_bundle_update().build();
        info!("bundle state lne: {}", test.bundle_state.state.len());
        info!(
            "transition state = {:?}",
            test.transition_state.as_ref().map_or(0, |t| t.transitions.len())
        );

        let mut evm = revm::Context::mainnet()
            .with_db(&mut test)
            .modify_block_chained(|b| {
                b.number = block.number;
                b.beneficiary = block.beneficiary;
                b.timestamp = block.timestamp;
                b.difficulty = block.header.difficulty;
                b.gas_limit = block.header.gas_limit;
                b.basefee = block.header.base_fee_per_gas.unwrap_or_default();
            })
            .build_mainnet();

        for (i, tx) in block.body.transactions().enumerate() {
            // info!("execute tx {i}");
            let tx = tx.try_clone_into_recovered().unwrap();
            let signer = tx.signer();
            let inner = tx.clone().into_inner();

            info!("signer = {signer:?}");

            evm.modify_tx(|etx| {
                etx.caller = signer;
                etx.gas_limit = inner.gas_limit();
                // etx.gas_price = inner.gas_price().unwrap_or(inner.max_fee_per_gas());
                etx.gas_price = inner.effective_gas_price(block.header.base_fee_per_gas);

                info!("\ngas price: {:?}", etx.gas_price);
                info!("effective gas price: {:?}", inner.effective_gas_price(None));

                etx.value = inner.value();
                etx.data = inner.input().to_owned();
                etx.gas_priority_fee = inner.max_priority_fee_per_gas();
                etx.max_fee_per_blob_gas = inner.max_fee_per_blob_gas().unwrap_or(u128::MAX);
                etx.chain_id = Some(1u64);
                etx.nonce = inner.nonce();
                if let Some(access_list) = inner.access_list() {
                    etx.access_list = access_list.clone()
                } else {
                    etx.access_list = Default::default();
                }

                etx.kind = match inner.to() {
                    Some(to_address) => TxKind::Call(to_address),
                    None => TxKind::Create,
                };
            });

            match evm.replay_commit() {
                // Ok(_) => info!("tx {i} executed succesfully"),
                Ok(_) => {}
                Err(error) => warn!("failed to execute tx {i}: {error:?}"),
            }
        }

        info!("bundle state len: {}", test.bundle_state.state.len());

        let transitions = test.transition_state.as_mut().expect("to exist").take();
        let bundle = test.bundle_state.apply_transitions_and_create_reverts(
            transitions,
            revm::database::states::bundle_state::BundleRetention::PlainState,
        );
        let mut bundle = test.take_bundle();

        for (account, state) in &bundle.state {
            info!("account: {account:?}, {:?}", state.status);
        }

        bundle.state.retain(|key, _| {
            key == &alloy_primitives::address!("0x000000629fbcf27a347d1aeba658435230d74a5f") ||
                key == &alloy_primitives::address!("0x037dd48ffd09fbdc1e385fefda48c6e1ef1382af") ||
                key == &alloy_primitives::address!("0x30daff27da012e118c07fae5380eb06f707c5ce4") ||
                key == &alloy_primitives::address!("0x3777261fd6e1ec0704735d491328215b9f5825b1") ||
                key == &alloy_primitives::address!("0x671e1c289f45ccaa82843501c7bc841ba26b97f1") ||
                key == &alloy_primitives::address!("0xf70da97812cb96acdf810712aa562db8dfa3dbef")
        });
        use std::str::FromStr;
        let mut value = bundle
            .state
            .get_mut(&alloy_primitives::address!("0x000000629fbcf27a347d1aeba658435230d74a5f"))
            .unwrap();
        value.info.as_mut().unwrap().balance =
            alloy_primitives::U256::from_str("63298440508785708615").unwrap();

        info!("bundle state len: {}", bundle.state.len());
        info!("{:#?}", bundle.state);

        let hashed: HashedPostState =
            HashedPostState::from_bundle_state::<KeccakKeyHasher>(&bundle.state);

        let test = input.parent_state.update(&hashed);
        let state_root = input.parent_state.state_root();

        info!(target: "a", "calculated state root = {state_root}");
        info!(target: "a", "actual state root     = {}", input.current_block.header().state_root());

        if state_root != input.current_block.header().state_root() {
            error!("invalid state root");
        }
    }
}

impl<C, P> BlockExecutor<C> for FullExecutor<C, P>
where
    C: ExecutorComponents,
    P: Provider<C::Network> + Clone,
{
    async fn execute(&self, block_number: u64) -> eyre::Result<()> {
        self.hooks.on_execution_start(block_number).await?;

        let client_input_from_cache = self.config.cache_dir.as_ref().and_then(|cache_dir| {
            match try_load_input_from_cache::<C::Primitives>(
                cache_dir,
                self.config.chain.id(),
                block_number,
            ) {
                Ok(client_input) => client_input,
                Err(e) => {
                    warn!("Failed to load input from cache: {}", e);
                    None
                }
            }
        });

        let client_input = match client_input_from_cache {
            Some(mut client_input_from_cache) => {
                // Override opcode tracking from cache by the setting provided by the user
                client_input_from_cache.opcode_tracking = self.config.opcode_tracking;
                client_input_from_cache
            }
            None => {
                let rpc_db = RpcDb::new(self.provider.clone(), block_number - 1);

                // Execute the host.
                let client_input = self
                    .host_executor
                    .execute(
                        block_number,
                        &rpc_db,
                        &self.provider,
                        self.config.genesis.clone(),
                        self.config.custom_beneficiary,
                        self.config.opcode_tracking,
                    )
                    .await?;

                warn!("INPUT SAVED");

                // let cache_dir = PathBuf::from("/tmp");
                // let input_folder = cache_dir.join(format!("input/{}", self.config.chain.id()));
                // if !input_folder.exists() {
                //     std::fs::create_dir_all(&input_folder)?;
                // }

                // let input_path = input_folder.join(format!("{}.bin", block_number));
                // let mut cache_file = std::fs::File::create(input_path)?;

                // bincode::serialize_into(&mut cache_file, &client_input)?;

                // TODO: calculate state root here

                client_input
            }
        };

        self.process_client(client_input, &self.hooks, self.config.prove_mode).await?;

        Ok(())
    }

    fn client(&self) -> Arc<C::Prover> {
        self.client.clone()
    }

    fn pk(&self) -> Arc<SP1ProvingKey> {
        self.pk.clone()
    }

    fn vk(&self) -> Arc<SP1VerifyingKey> {
        self.vk.clone()
    }
}

impl<C, P> Debug for FullExecutor<C, P>
where
    C: ExecutorComponents,
    P: Provider<C::Network> + Clone,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FullExecutor").field("config", &self.config).finish()
    }
}

pub struct CachedExecutor<C>
where
    C: ExecutorComponents,
{
    cache_dir: PathBuf,
    chain_id: u64,
    client: Arc<C::Prover>,
    pk: Arc<SP1ProvingKey>,
    vk: Arc<SP1VerifyingKey>,
    hooks: C::Hooks,
    prove_mode: Option<SP1ProofMode>,
}

impl<C> CachedExecutor<C>
where
    C: ExecutorComponents,
{
    pub async fn try_new(
        elf: Vec<u8>,
        client: Arc<C::Prover>,
        hooks: C::Hooks,
        cache_dir: PathBuf,
        chain_id: u64,
        prove_mode: Option<SP1ProofMode>,
    ) -> eyre::Result<Self> {
        let cloned_client = client.clone();

        // Setup the proving key and verification key.
        let (pk, vk) = task::spawn_blocking(move || {
            let (pk, vk) = cloned_client.setup(&elf);
            (pk, vk)
        })
        .await?;

        Ok(Self {
            cache_dir,
            chain_id,
            client,
            pk: Arc::new(pk),
            vk: Arc::new(vk),
            hooks,
            prove_mode,
        })
    }
}

impl<C> BlockExecutor<C> for CachedExecutor<C>
where
    C: ExecutorComponents,
{
    async fn execute(&self, block_number: u64) -> eyre::Result<()> {
        let client_input = try_load_input_from_cache::<C::Primitives>(
            &self.cache_dir,
            self.chain_id,
            block_number,
        )?
        .ok_or(eyre::eyre!("No cached input found"))?;

        self.process_client(client_input, &self.hooks, self.prove_mode).await
    }

    fn client(&self) -> Arc<C::Prover> {
        self.client.clone()
    }

    fn pk(&self) -> Arc<SP1ProvingKey> {
        self.pk.clone()
    }

    fn vk(&self) -> Arc<SP1VerifyingKey> {
        self.vk.clone()
    }
}

impl<C> Debug for CachedExecutor<C>
where
    C: ExecutorComponents,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedExecutor").field("cache_dir", &self.cache_dir).finish()
    }
}

// Block execution in SP1 is a long-running, blocking task, so run it in a separate thread.
async fn execute_client<P: Prover<CpuProverComponents> + 'static>(
    number: u64,
    client: Arc<P>,
    pk: Arc<SP1ProvingKey>,
    stdin: SP1Stdin,
) -> eyre::Result<(SP1Stdin, eyre::Result<(SP1PublicValues, ExecutionReport)>)> {
    task::spawn_blocking(move || {
        info_span!("execute_client", number).in_scope(|| {
            let result = client.execute(&pk.elf, &stdin);
            (stdin, result.map_err(|err| eyre::eyre!("{err}")))
        })
    })
    .await
    .map_err(|err| eyre::eyre!("{err}"))
}

fn try_load_input_from_cache<P: NodePrimitives + DeserializeOwned>(
    cache_dir: &Path,
    chain_id: u64,
    block_number: u64,
) -> eyre::Result<Option<ClientExecutorInput<P>>> {
    let cache_path = cache_dir.join(format!("input/{}/{}.bin", chain_id, block_number));

    if cache_path.exists() {
        // TODO: prune the cache if invalid instead
        let mut cache_file = std::fs::File::open(cache_path)?;
        let client_input = bincode::deserialize_from(&mut cache_file)?;

        Ok(Some(client_input))
    } else {
        Ok(None)
    }
}
