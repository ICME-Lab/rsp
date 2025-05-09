# NovaNet ETH proofs

Modified version of rsp to get state export working. Idea is that code that builds the state (`HostExecutor::execute()`) could be copied to NovaNet's eth-proofs host implementation, minimizing `ClientExecutorInput` to something that could be deserialized in WASM and the implementing a block executor with revm that takes `ClientExecutorInput`, deserializes it and executes the block with revm in WASM. PoC of the WASM executor is currently being developed in `FullExecutor::test()` function.

### prove block with revm:
```
cargo run --profile release --bin eth-proofs -- --ws-rpc-url <URL> --http-rpc-url <URL>
````

#### modifications made in `bin/eth-proofs/src/main.rs`:

* uncomment `// executor.test().await;` to execute a block with revm (`crates/executor/host/src/full_executor.rs#231`)
* uncomment `// executor.execute(20526624u64).await;` to execute a block reth (`crates/executor/host/src/host_executor.rs#63`)

see the comments above these two functions for more details. Essentially `executor.test().await` is a WIP function that attempts to execute an ETH block with revm, using the state built by reth (`./20526624.bin`). `executor.execute(20526624u64).await` can be used to build new states for different blocks and produce state roots which can then be compared against the state roots calculated by `executor.test().await`.

current issues:
   * post-state of the client executor (`bin/client/src/main.rs`) that uses the pre-generated state contains *79* state modifications
   * post-state of the revm executor (`crates/executor/host/src/full_executor.rs FullExecutor::test()`) that uses the pre-generated state contains *77* state modifications, i.e., missing two accounts

`./output1` contains a list of accounts that revm modified during execution, `./output3` contains a list of accounts that client executor modified during execution

`0x000f3df6d732807ef1319fb7b8bb8522d0beac02, Changed` and `0xf5a2ecec8333bb295569bb40f41918d3073ccab2, Changed` are missing. Why they're missing from revm's post-state is not known yet.


### current issue under investigation

transaction which currently causes issues: https://etherscan.io/tx/0x9e99b84038e3c67b604826850c105f9e6b0e4328899c69c5fefe98ce4db6f83e

the tx contains 5 blobs and the post-state for the account that sent this tx (`0x6887246668a3b87f54deb3b94ba47a6f63f32985`) has different balance between revm and reth. The tx should deduct data fee (`655,360`) from the balance and it's correctly deducted when run with reth but revm doesn't deduct this, causing there to be a mismatch in balance, causing state root mismatch between revm and reth. Essentially revm doesn't deduct the data fee even though it probably should.

this account is not the only issue and there is at least one more (could be the two missing accounts) because excluding `0x6887246668a3b87f54deb3b94ba47a6f63f32985` from state root calculation still results in state root mismatch.

if the post-state only includes the following accounts, state roots matches between reth and revm:
* 0x000000629fbcf27a347d1aeba658435230d74a5f
* 0x037dd48ffd09fbdc1e385fefda48c6e1ef1382af
* 0x06a9ab27c7e2255df1815e6cc0168d7755feb19a
* 0x08e96f308eb008b3db68640aba6b06078625f8cd
* 0x0d0707963952f2fba59dd06f2b425ace40b492fe
* 0x111111125421ca6dc452d289314280a0f8842a65
* 0x12106758e03613e66fa96209927940c825e85fff
* 0x1516008376543c283654f60b03a28e1c9930806a
* 0x16c0829dd60124f2a7d49a5e768f7978a57c2393
* 0x1728d7099f6535f5efeba784a4ba54120ceada6b
* 0x1d71eb5d4f05884add4d8e8a4d31eef3a4263c47
* 0x23529b46bb5fdb9f9d0427e9a35115551b72581b
* 0x239426c2feda17d10635b6e7d1cfca9ab33ab222
* 0x26c1087b6a658c106768eea1931e083ce469f20c
* 0x30daff27da012e118c07fae5380eb06f707c5ce4
* 0x340d2bde5eb28c1eed91b2f790723e3b160613b7
* 0x3777261fd6e1ec0704735d491328215b9f5825b1
* 0x4280b10e7cd12171e944401e4018250d2052a0d6
* 0x4a5565db6515923418bb9ab1a8ad816e85c12ff4
* 0x4cff49d0a19ed6ff845a9122fa912abcfb1f68a6
* 0x4d224452801aced8b2f0aebe155379bb5d594381
* 0x4d9ff50ef4da947364bb9650892b2554e7be5e2b
* 0x5c9538085fdfce7470e66f7c3e1b1f0f01d969aa
* 0x5faa989af96af85384b8a938c2ede4a7378d9875
* 0x671e1c289f45ccaa82843501c7bc841ba26b97f1

meaning it is possible under certain conditions to get a state root match between revm and reth. Rest of the accounts have not been tested yet.

### debugging state root mismatches

* add new account to `FullExecutor::test()#350 bundle.state.retain(...)`, uncomment `executor.test().await` and run `cargo run ...` (see above)
  * prints `calculated state root = 0x006c250904cb160c5fd51724900cfd967d22141ca1accc9e52f9a4b82e1a3d8c`
* add the same account to `HostExecutor::execute()#194 bundle.state.retain(...)`, uncomment `executor.execute(20526624u64).await` and run `cargo run ...`
   * prints `state root = 0x006c250904cb160c5fd51724900cfd967d22141ca1accc9e52f9a4b82e1a3d8c`

if the state roots match, repeat process. If they do not match, inspect the post-states between revm and reth:

`FullExecutor::test()#388-392`:

```rust
let mut value = bundle
   .state
   .get_mut(&alloy_primitives::address!("0x6887246668a3b87f54deb3b94ba47a6f63f32985"))
   .unwrap();
info!("state for 0x6887246668a3b87f54deb3b94ba47a6f63f32985: {value:#?}");
```

`HostExecutor::execute()#231-236`:

```rust
tracing::info!(
  "state for 0x6887246668a3b87f54deb3b94ba47a6f63f32985: {:#?}",
  executor_outcome.bundle.state.get(&alloy_primitives::address!(
      "0x6887246668a3b87f54deb3b94ba47a6f63f32985"
  ))
);
```

for `0x6887246668a3b87f54deb3b94ba47a6f63f32985` there is a balance mismatch because of the data blobs being ignored, resulting in a state root mismatch.

whether revm can deduct them is unsure, this what the developer commented on an unrelated issue in Github: https://github.com/bluealloy/revm/issues/1370#issuecomment-2094685067

modifying the blob gas price for tx (`modify_tx()`) or block (`modify_block_chained()`) does not seem to have an effect. The documnetation for revm is very limited and the example code resulted in incorrect gas fees being deducted (`etx.gas_price = tx.gas_price().unwrap_or(tx.inner.max_fee_per_gas());` vs `inner.effective_gas_price(block.header.base_fee_per_gas)`) so all of the examples cannot be trusted fully either.

---
# Reth Succinct Processor (RSP)

A minimal implementation of generating zero-knowledge proofs of EVM block execution using [Reth](https://github.com/paradigmxyz/reth). Supports both Ethereum and OP Stack.

> [!CAUTION]
>
> This repository is still an active work-in-progress and is not audited or meant for production usage.

## Getting Started

To use RSP, you must first have [Rust](https://www.rust-lang.org/tools/install) installed and [SP1](https://docs.succinct.xyz/docs/sp1/getting-started/install) installed to build the client programs. Then follow the instructions below.

### Installing the CLI

In the root directory of this repository, run:

```console
cargo install --locked --path bin/host
```

and the command `rsp` will be installed.

### RPC Node Requirement

RSP fetches block and state data from a JSON-RPC node. You must use an archive node which preserves historical intermediate trie nodes needed for fetching storage proofs.

In Geth, the archive mode can be enabled with the `--gcmode=archive` option. You can also use an RPC provider that offers archive data access.

> [!IMPORTANT]  
>
> Some RPC providers have issues with `eth_getProof` on older blocks. For instance QuickNode returns invalid data that lead to state mismatch errors.

> [!TIP]
>
> Don't have access to such a node but still want to try out RSP? Use [`rsp-tests`](https://github.com/succinctlabs/rsp-tests) to get quickly set up with an offline cache built for selected blocks.

### Running the CLI

For the supported chains (Ethereum Mainnet and Sepolia, OP Stack Mainnet, and Linea Mainnet), the host CLI automatically identifies the underlying chain type using the RPC (with the `eth_chainId` call). Simply supply a block number and an RPC URL:

```console
rsp --block-number 18884864 --rpc-url <RPC>
```

If you want to run RSP on another EVM chain, you must specify the genesis JSON file with `--genesis-path`:

```console
rsp --block-number 18884864 --rpc-url <RPC> --genesis-path <GENESIS_PATH>
```

> [!TIP]
>
> The genesis json file only need to contains the chain id and hardforks block/timestamps. You can have a look at the folder 
> `bin/host/genesis` for examples.

When running RSP, you should see logs similar to:

```log
2024-07-15T00:49:03.857638Z  INFO rsp_host_executor: fetching the current block and the previous block
2024-07-15T00:49:04.547738Z  INFO rsp_host_executor: setting up the spec for the block executor
2024-07-15T00:49:04.551198Z  INFO rsp_host_executor: setting up the database for the block executor
2024-07-15T00:49:04.551268Z  INFO rsp_host_executor: executing the block and with rpc db: block_number=18884864, transaction_count=30
2024-07-15T00:50:51.526624Z  INFO rsp_host_executor: verifying the state root
...
```

The host CLI executes the block while fetching additional data necessary for offline execution. The same execution and verification logic is then run inside the zkVM. No actual proof is generated from this command, but it will print out a detailed execution report and statistics on the # of cycles to a CSV file (can be specified by the `--report-path` argument).

Additional information about precompiles can be added to the CSV file when specifying the `--precompile-tracking` argument, and about opcodes with the `--opcode-tracking` argument.

You can also run the CLI directly by running the following command:

```bash
cargo run --bin rsp --release -- --block-number 18884864 --rpc-url <RPC>
```

or by providing the RPC URL in the `.env` file (or otherwise setting the relevant env vars) and specifying the chain id in the CLI command like this:

```bash
cargo run --bin rsp --release -- --block-number 18884864 --chain-id <chain-id>
```

#### Chain using the Clique consensus

If you want to run RSP on a chain using the Clique consensus (for instance Linea), you will have to specify the the block beneficiary as the `--custom-beneficiary` CLI argument, as Clique is not implemented in reth.

#### Using cached client input

The client input (witness) generated by executing against RPC can be cached to speed up iteration of the client program by supplying the `--cache-dir` option:

```bash
cargo run --bin rsp --release -- --block-number 18884864 --chain-id <chain-id> --cache-dir /path/to/cache
```

Note that even when utilizing a cached input, the host still needs access to the chain ID to identify the network type, either through `--rpc-url` or `--chain-id`. To run the host completely offline, use `--chain-id` for this.

## Running Tests

End-to-end integration tests are available. To run these tests, utilize the `.env` file (see [example](./.env.example)) or manually set these environment variables:

```bash
export RPC_1="YOUR_ETHEREUM_MAINNET_RPC_URL"
export RPC_10="YOUR_OP_MAINNET_RPC_URL"
export RPC_59144="YOUR_LINEA_MAINNET_RPC_URL"
export RPC_11155111="YOUR_SEPOLIA_RPC_URL"
```

Note that these JSON-RPC nodes must fulfill the [RPC node requirement](#rpc-node-requirement).

Then execute:

```bash
RUST_LOG=info cargo test -p rsp-host-executor --release e2e -- --nocapture
```

### Generating Proofs

If you want to actually generate proofs, you can run the CLI using the `--prove` argument, like this:

```bash
cargo run --bin rsp --release -- --block-number 18884864 --chain-id <chain-id> --prove
```

This will generate proofs locally on your machine. Given how large these programs are, it might take a while for the proof to generate.

#### Run with prover network

If you want to run proofs using Succinct's [prover network](https://docs.succinct.xyz/docs/sp1/generating-proofs/prover-network), follow the sign-up instructions, and run the command with the following environment variables prefixed:

```bash
SP1_PROVER=network SP1_PRIVATE_KEY=
```

To specify a custom prover network RPC, you can use the `PROVER_NETWORK_RPC` environment variable.

#### Run with GPU

To generate proofs locally on a GPU, you can enable the `cuda` feature in the CLI, which will enable it in the SDK. Make sure to read the instructions [here](https://github.com/succinctlabs/sp1/blob/fb967e8c409b318d18985f8f92353e93d38c7cda/book/generating-proofs/hardware-acceleration/cuda.md) to make sure you have all required dependencies installed. You can run it with a command like this:

```bash
cargo run --bin rsp --release --features cuda -- --block-number 18884864 --chain-id <chain-id> --prove
```

#### Benchmarking on ETH proofs

To run benchmarking with [ETH proofs](https://staging--ethproofs.netlify.app/), you'll need to:

1. Set the following environment variables:
   ```bash
   export ETH_PROOFS_ENDPOINT="https://staging--ethproofs.netlify.app/api/v0"
   export ETH_PROOFS_API_TOKEN=<your_api_token>
   export RPC_URL=<your_eth_mainnet_rpc>
   ```

3. Run the benchmarking recipe:
   ```bash
   # Run with default cluster ID (1) and sleep time (900s)
   just run-eth-proofs

   # Run with custom cluster ID and sleep time (in seconds)
   just run-eth-proofs 5 600
   ```

This will continuously:
- Fetch the latest block number
- Round it down to the nearest 100
- Generate a proof and submit its proving time
- Sleep for the specified duration before the next iteration

## FAQ

### Building the client programs manually

By default, the `build.rs` in the `bin/host` crate will rebuild the client programs every time they are modified. To manually build the client programs, you can run these commands (ake sure you have the [SP1 toolchain](https://docs.succinct.xyz/docs/sp1/getting-started/install) installed):

```console
cd ./bin/client-eth
cargo prove build --ignore-rust-version
```

To build the Optimism client ELF program:

```console
cd ./bin/client-op
cargo prove build --ignore-rust-version
```

### What are good testing blocks

A good small block to test on for Ethereum mainnet is: `20526624`.

### State root mismatch

This issue can be caused using an RPC provider that returns incorrect results from the `eth_getProof` endpoint. We have empirically observed such issues with many RPC providers. We recommend using Alchemy.
