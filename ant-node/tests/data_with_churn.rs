// Copyright 2024 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

mod common;

use crate::common::{
    client::{get_client_and_funded_wallet, get_node_count},
    NodeRestart,
};
use ant_logging::LogBuilder;
use ant_protocol::{storage::ChunkAddress, NetworkAddress};
use autonomi::{Client, Wallet};
use common::client::transfer_to_new_wallet;
use eyre::{bail, ErrReport, Result};
use rand::Rng;
use self_encryption::MAX_CHUNK_SIZE;
use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    fs::create_dir_all,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};
use tempfile::tempdir;
use test_utils::gen_random_data;
use tokio::{sync::RwLock, task::JoinHandle, time::sleep};
use tracing::{debug, error, info, trace, warn};
use xor_name::XorName;

const TOKENS_TO_TRANSFER: usize = 10000000;

const EXTRA_CHURN_COUNT: u32 = 5;
const CHURN_CYCLES: u32 = 2;
const CHUNK_CREATION_RATIO_TO_CHURN: u32 = 15;
const REGISTER_CREATION_RATIO_TO_CHURN: u32 = 15;

static DATA_SIZE: LazyLock<usize> = LazyLock::new(|| *MAX_CHUNK_SIZE / 3);

const CONTENT_QUERY_RATIO_TO_CHURN: u32 = 40;
const MAX_NUM_OF_QUERY_ATTEMPTS: u8 = 5;

// Default total amount of time we run the checks for before reporting the outcome.
// It can be overriden by setting the 'TEST_DURATION_MINS' env var.
const TEST_DURATION: Duration = Duration::from_secs(60 * 60); // 1hr

type ContentList = Arc<RwLock<VecDeque<NetworkAddress>>>;

struct ContentError {
    net_addr: NetworkAddress,
    attempts: u8,
    last_err: ErrReport,
}

impl fmt::Debug for ContentError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{:?}, attempts: {}, last error: {:?}",
            self.net_addr, self.attempts, self.last_err
        )
    }
}

type ContentErredList = Arc<RwLock<BTreeMap<NetworkAddress, ContentError>>>;

#[tokio::test(flavor = "multi_thread")]
async fn data_availability_during_churn() -> Result<()> {
    let _log_appender_guard = LogBuilder::init_multi_threaded_tokio_test("data_with_churn", false);

    let test_duration = if let Ok(str) = std::env::var("TEST_DURATION_MINS") {
        Duration::from_secs(60 * str.parse::<u64>()?)
    } else {
        TEST_DURATION
    };
    let node_count = get_node_count();

    let churn_period = if let Ok(str) = std::env::var("TEST_TOTAL_CHURN_CYCLES") {
        println!("Using value set in 'TEST_TOTAL_CHURN_CYCLES' env var: {str}");
        info!("Using value set in 'TEST_TOTAL_CHURN_CYCLES' env var: {str}");
        let cycles = str.parse::<u32>()?;
        test_duration / cycles
    } else {
        // Ensure at least some nodes got churned twice.
        test_duration
            / std::cmp::max(
                CHURN_CYCLES * node_count as u32,
                node_count as u32 + EXTRA_CHURN_COUNT,
            )
    };
    println!("Nodes will churn every {churn_period:?}");
    info!("Nodes will churn every {churn_period:?}");

    // Create a cross thread usize for tracking churned nodes
    let churn_count = Arc::new(RwLock::new(0_usize));

    // Allow to disable Registers data creation/checks, storing and querying only Chunks during churn.
    // Default to be not carry out chunks only during churn.
    let chunks_only = std::env::var("CHUNKS_ONLY").is_ok();

    println!(
        "Running this test for {test_duration:?}{}...",
        if chunks_only { " (Chunks only)" } else { "" }
    );
    info!(
        "Running this test for {test_duration:?}{}...",
        if chunks_only { " (Chunks only)" } else { "" }
    );

    let (client, main_wallet) = get_client_and_funded_wallet().await;

    info!(
        "Client and wallet created. Main wallet address: {:?}",
        main_wallet.address()
    );

    // Shared bucket where we keep track of content created/stored on the network
    let content = ContentList::default();

    // Spawn a task to create Registers and CashNotes at random locations,
    // at a higher frequency than the churning events
    let create_register_handle = if !chunks_only {
        let register_wallet = transfer_to_new_wallet(&main_wallet, TOKENS_TO_TRANSFER).await?;
        let create_register_handle = create_registers_task(
            client.clone(),
            register_wallet,
            Arc::clone(&content),
            churn_period,
        );
        Some(create_register_handle)
    } else {
        None
    };

    println!("Uploading some chunks before carry out node churning");
    info!("Uploading some chunks before carry out node churning");

    let chunk_wallet = transfer_to_new_wallet(&main_wallet, TOKENS_TO_TRANSFER).await?;
    // Spawn a task to store Chunks at random locations, at a higher frequency than the churning events
    let store_chunks_handle = store_chunks_task(
        client.clone(),
        chunk_wallet,
        Arc::clone(&content),
        churn_period,
    );

    // Spawn a task to churn nodes
    churn_nodes_task(Arc::clone(&churn_count), test_duration, churn_period);

    // Shared bucket where we keep track of the content which erred when creating/storing/fetching.
    // We remove them from this bucket if we are then able to query/fetch them successfully.
    // We only try to query them 'MAX_NUM_OF_QUERY_ATTEMPTS' times, then report them effectivelly as failures.
    let content_erred = ContentErredList::default();

    // Shared bucket where we keep track of the content we failed to fetch for 'MAX_NUM_OF_QUERY_ATTEMPTS' times.
    let failures = ContentErredList::default();

    // Spawn a task to randomly query/fetch the content we create/store
    query_content_task(
        client.clone(),
        Arc::clone(&content),
        Arc::clone(&content_erred),
        churn_period,
    );

    // Spawn a task to retry querying the content that failed, up to 'MAX_NUM_OF_QUERY_ATTEMPTS' times,
    // and mark them as failures if they effectivelly cannot be retrieved.
    retry_query_content_task(
        client.clone(),
        Arc::clone(&content_erred),
        Arc::clone(&failures),
        churn_period,
    );

    info!("All tasks have been spawned. The test is now running...");
    println!("All tasks have been spawned. The test is now running...");

    let start_time = Instant::now();
    while start_time.elapsed() < test_duration {
        if store_chunks_handle.is_finished() {
            bail!("Store chunks task has finished before the test duration. Probably due to an error.");
        }
        if let Some(handle) = &create_register_handle {
            if handle.is_finished() {
                bail!("Create registers task has finished before the test duration. Probably due to an error.");
            }
        }

        let failed = failures.read().await;
        if start_time.elapsed().as_secs() % 10 == 0 {
            println!(
                "Current failures after {:?} ({}): {:?}",
                start_time.elapsed(),
                failed.len(),
                failed.values()
            );
            info!(
                "Current failures after {:?} ({}): {:?}",
                start_time.elapsed(),
                failed.len(),
                failed.values()
            );
        }

        sleep(Duration::from_secs(3)).await;
    }

    println!();
    println!(
        ">>>>>> Test stopping after running for {:?}. <<<<<<",
        start_time.elapsed()
    );
    println!("{:?} churn events happened.", *churn_count.read().await);
    println!();

    // The churning of storing_chunk/querying_chunk are all random,
    // which will have a high chance that newly stored chunk got queried BEFORE
    // the original holders churned out.
    // i.e. the test may pass even without any replication
    // Hence, we carry out a final round of query all data to confirm storage.
    println!("Final querying confirmation of content");
    info!("Final querying confirmation of content");

    // take one read lock to avoid holding the lock for the whole loop
    // prevent any late content uploads being added to the list
    let content = content.read().await;
    let uploaded_content_count = content.len();
    let mut handles = Vec::new();
    for net_addr in content.iter() {
        let client = client.clone();
        let net_addr = net_addr.clone();

        let failures = Arc::clone(&failures);
        let handle = tokio::spawn(async move {
            final_retry_query_content(&client, &net_addr, churn_period, failures).await
        });
        handles.push(handle);
    }
    let results: Vec<_> = futures::future::join_all(handles).await;

    let content_queried_count = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(
        content_queried_count, uploaded_content_count,
        "Not all content was queried successfully"
    );

    println!("{content_queried_count:?} pieces of content queried");

    assert_eq!(
        content_queried_count, uploaded_content_count,
        "Not all content was queried"
    );

    let failed = failures.read().await;
    if failed.len() > 0 {
        bail!("{} failure/s in test: {:?}", failed.len(), failed.values());
    }

    println!("Test passed after running for {:?}.", start_time.elapsed());
    Ok(())
}

// Spawns a task which periodically creates Registers at random locations.
fn create_registers_task(
    client: Client,
    wallet: Wallet,
    content: ContentList,
    churn_period: Duration,
) -> JoinHandle<Result<()>> {
    let handle: JoinHandle<Result<()>> = tokio::spawn(async move {
        // Create Registers at a higher frequency than the churning events
        let delay = churn_period / REGISTER_CREATION_RATIO_TO_CHURN;

        loop {
            let owner = Client::register_generate_key();
            let random_name = XorName(rand::random()).to_string();
            let random_data = gen_random_data(*DATA_SIZE);

            sleep(delay).await;

            let mut retries = 1;
            loop {
                match client
                    .register_create(random_data.clone(), &random_name, owner.clone(), &wallet)
                    .await
                {
                    Ok(register) => {
                        let addr = register.address();
                        println!("Created new Register ({addr:?}) after a delay of: {delay:?}");
                        content
                            .write()
                            .await
                            .push_back(NetworkAddress::RegisterAddress(*addr));
                        break;
                    }
                    Err(err) => {
                        println!("Failed to create register: {err:?}. Retrying ...");
                        error!("Failed to create register: {err:?}. Retrying ...");
                        if retries >= 3 {
                            println!("Failed to create register after 3 retries: {err}");
                            error!("Failed to create register after 3 retries: {err}");
                            bail!("Failed to create register after 3 retries: {err}");
                        }
                        retries += 1;
                    }
                }
            }
        }
    });
    handle
}

// Spawns a task which periodically stores Chunks at random locations.
fn store_chunks_task(
    client: Client,
    wallet: Wallet,
    content: ContentList,
    churn_period: Duration,
) -> JoinHandle<Result<()>> {
    let handle: JoinHandle<Result<()>> = tokio::spawn(async move {
        let temp_dir = tempdir().expect("Can not create a temp directory for store_chunks_task!");
        let output_dir = temp_dir.path().join("chunk_path");
        create_dir_all(output_dir.clone())
            .expect("failed to create output dir for encrypted chunks");

        // Store Chunks at a higher frequency than the churning events
        let delay = churn_period / CHUNK_CREATION_RATIO_TO_CHURN;

        loop {
            let random_data = gen_random_data(*DATA_SIZE);

            // FIXME: The client does not have the retry repay to different payee feature yet.
            // Retry here for now
            let mut retries = 1;
            loop {
                match client
                    .data_put(random_data.clone(), (&wallet).into())
                    .await
                    .inspect_err(|err| {
                        println!("Error to put chunk: {err:?}");
                        error!("Error to put chunk: {err:?}")
                    }) {
                    Ok(data_map) => {
                        println!("Stored Chunk/s at {data_map:?} after a delay of: {delay:?}");
                        info!("Stored Chunk/s at {data_map:?} after a delay of: {delay:?}");

                        content
                            .write()
                            .await
                            .push_back(NetworkAddress::ChunkAddress(ChunkAddress::new(data_map)));
                        break;
                    }
                    Err(err) => {
                        println!("Failed to store chunk: {err:?}. Retrying ...");
                        error!("Failed to store chunk: {err:?}. Retrying ...");
                        if retries >= 3 {
                            println!("Failed to store chunk after 3 retries: {err}");
                            error!("Failed to store chunk after 3 retries: {err}");
                            bail!("Failed to store chunk after 3 retries: {err}");
                        }
                        retries += 1;
                    }
                }
            }

            sleep(delay).await;
        }
    });
    handle
}

// Spawns a task which periodically queries a content by randomly choosing it from the list
// of content created by another task.
fn query_content_task(
    client: Client,
    content: ContentList,
    content_erred: ContentErredList,
    churn_period: Duration,
) {
    let _handle = tokio::spawn(async move {
        let delay = churn_period / CONTENT_QUERY_RATIO_TO_CHURN;
        loop {
            let len = content.read().await.len();
            if len == 0 {
                println!("No content created/stored just yet, let's try in {delay:?} ...");
                info!("No content created/stored just yet, let's try in {delay:?} ...");
                sleep(delay).await;
                continue;
            }

            // let's choose a random content to query, picking it from the list of created
            let index = rand::thread_rng().gen_range(0..len);
            let net_addr = content.read().await[index].clone();
            trace!("Querying content (bucket index: {index}) at {net_addr:?} in {delay:?}");
            sleep(delay).await;

            match query_content(&client, &net_addr).await {
                Ok(_) => {
                    let _ = content_erred.write().await.remove(&net_addr);
                }
                Err(last_err) => {
                    println!(
                        "Failed to query content (index: {index}) at {net_addr}: {last_err:?}"
                    );
                    error!("Failed to query content (index: {index}) at {net_addr}: {last_err:?}");
                    // mark it to try 'MAX_NUM_OF_QUERY_ATTEMPTS' times.
                    let _ = content_erred
                        .write()
                        .await
                        .entry(net_addr.clone())
                        .and_modify(|curr| curr.attempts += 1)
                        .or_insert(ContentError {
                            net_addr,
                            attempts: 1,
                            last_err,
                        });
                }
            }
        }
    });
}

// Spawns a task which periodically picks up a node, and restarts it to cause churn in the network.
fn churn_nodes_task(
    churn_count: Arc<RwLock<usize>>,
    test_duration: Duration,
    churn_period: Duration,
) {
    let start = Instant::now();
    let _handle: JoinHandle<Result<()>> = tokio::spawn(async move {
        let mut node_restart = NodeRestart::new(true, false)?;

        loop {
            sleep(churn_period).await;

            // break out if we've run the duration of churn
            if start.elapsed() > test_duration {
                debug!("Test duration reached, stopping churn nodes task");
                break;
            }

            if let Err(err) = node_restart.restart_next(true, true).await {
                println!("Failed to restart node {err}");
                info!("Failed to restart node {err}");
                continue;
            }

            *churn_count.write().await += 1;
        }
        Ok(())
    });
}

// Checks (periodically) for any content that an error was reported either at the moment of its creation or
// in a later query attempt.
fn retry_query_content_task(
    client: Client,
    content_erred: ContentErredList,
    failures: ContentErredList,
    churn_period: Duration,
) {
    let _handle = tokio::spawn(async move {
        let delay = 2 * churn_period;
        loop {
            sleep(delay).await;

            // let's try to query from the bucket of those that erred upon creation/query
            let erred = content_erred.write().await.pop_first();

            if let Some((net_addr, mut content_error)) = erred {
                let attempts = content_error.attempts + 1;

                println!("Querying erred content at {net_addr}, attempt: #{attempts} ...");
                info!("Querying erred content at {net_addr}, attempt: #{attempts} ...");
                if let Err(last_err) = query_content(&client, &net_addr).await {
                    println!("Erred content is still not retrievable at {net_addr} after {attempts} attempts: {last_err:?}");
                    warn!("Erred content is still not retrievable at {net_addr} after {attempts} attempts: {last_err:?}");
                    // We only keep it to retry 'MAX_NUM_OF_QUERY_ATTEMPTS' times,
                    // otherwise report it effectivelly as failure.
                    content_error.attempts = attempts;
                    content_error.last_err = last_err;

                    if attempts == MAX_NUM_OF_QUERY_ATTEMPTS {
                        let _ = failures.write().await.insert(net_addr, content_error);
                    } else {
                        let _ = content_erred.write().await.insert(net_addr, content_error);
                    }
                } else {
                    // remove from fails and errs if we had a success and it was added meanwhile perchance
                    let _ = failures.write().await.remove(&net_addr);
                    let _ = content_erred.write().await.remove(&net_addr);
                }
            }
        }
    });
}

async fn final_retry_query_content(
    client: &Client,
    net_addr: &NetworkAddress,
    churn_period: Duration,
    failures: ContentErredList,
) -> Result<()> {
    let mut attempts = 1;
    let net_addr = net_addr.clone();
    loop {
        println!("Final querying content at {net_addr}, attempt: #{attempts} ...");
        debug!("Final querying content at {net_addr}, attempt: #{attempts} ...");
        if let Err(last_err) = query_content(client, &net_addr).await {
            if attempts == MAX_NUM_OF_QUERY_ATTEMPTS {
                println!("Final check: Content is still not retrievable at {net_addr} after {attempts} attempts: {last_err:?}");
                error!("Final check: Content is still not retrievable at {net_addr} after {attempts} attempts: {last_err:?}");
                bail!("Final check: Content is still not retrievable at {net_addr} after {attempts} attempts: {last_err:?}");
            } else {
                attempts += 1;
                let delay = 2 * churn_period;
                debug!("Delaying last check for {delay:?} ...");
                sleep(delay).await;
                continue;
            }
        } else {
            failures.write().await.remove(&net_addr);
            // content retrieved fine
            return Ok(());
        }
    }
}

async fn query_content(client: &Client, net_addr: &NetworkAddress) -> Result<()> {
    match net_addr {
        NetworkAddress::RegisterAddress(addr) => {
            let _ = client.register_get(*addr).await?;
            Ok(())
        }
        NetworkAddress::ChunkAddress(addr) => {
            client.data_get(*addr.xorname()).await?;
            Ok(())
        }
        _other => Ok(()), // we don't create/store any other type of content in this test yet
    }
}