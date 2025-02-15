/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

#![allow(clippy::significant_drop_in_scrutinee)] // FIXME?

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use allocative::Allocative;
use anyhow::Context;
use buck2_common::file_ops::IgnoreSet;
use buck2_common::invocation_paths::InvocationPaths;
use buck2_common::io::IoProvider;
use buck2_common::legacy_configs::cells::BuckConfigBasedCells;
use buck2_common::legacy_configs::LegacyBuckConfig;
use buck2_common::result::SharedResult;
use buck2_common::result::ToSharedResultExt;
use buck2_core::async_once_cell::AsyncOnceCell;
use buck2_core::cells::CellName;
use buck2_core::facebook_only;
use buck2_core::fs::project::ProjectRelativePathBuf;
use buck2_core::fs::project::ProjectRoot;
use buck2_core::rollout_percentage::RolloutPercentage;
use buck2_events::dispatch::EventDispatcher;
use buck2_events::sink::scribe;
use buck2_events::sink::tee::TeeSink;
use buck2_events::trace::TraceId;
use buck2_events::EventSource;
use buck2_execute::execute::blocking::BlockingExecutor;
use buck2_execute::execute::blocking::BuckBlockingExecutor;
use buck2_execute::materialize::materializer::MaterializationMethod;
use buck2_execute::materialize::materializer::Materializer;
use buck2_execute::re::client::RemoteExecutionStaticMetadata;
use buck2_execute::re::manager::ReConnectionManager;
use buck2_execute_impl::materializers::deferred::DeferredMaterializer;
use buck2_execute_impl::materializers::deferred::DeferredMaterializerConfigs;
use buck2_execute_impl::materializers::immediate::ImmediateMaterializer;
use buck2_execute_impl::materializers::sqlite::MaterializerState;
use buck2_execute_impl::materializers::sqlite::MaterializerStateSqliteDb;
use buck2_forkserver::client::ForkserverClient;
use buck2_server_ctx::concurrency::ConcurrencyHandler;
use buck2_server_ctx::concurrency::NestedInvocation;
use buck2_server_ctx::concurrency::ParallelInvocation;
use cli_proto::unstable_dice_dump_request::DiceDumpFormat;
use dice::Dice;
use fbinit::FacebookInit;
use gazebo::dupe::Dupe;
use gazebo::variants::VariantName;
use tokio::sync::Mutex;

use crate::active_commands::ActiveCommandDropGuard;
use crate::ctx::BaseServerCommandContext;
use crate::daemon::check_working_dir;
use crate::daemon::disk_state::delete_unknown_disk_state;
use crate::daemon::disk_state::maybe_load_or_initialize_materializer_sqlite_db;
use crate::daemon::disk_state::DiskStateOptions;
use crate::daemon::forkserver::maybe_launch_forkserver;
use crate::daemon::panic::DaemonStatePanicDiceDump;
use crate::file_watcher::FileWatcher;

/// For a buckd process there is a single DaemonState created at startup and never destroyed.
#[derive(Allocative)]
pub struct DaemonState {
    #[allocative(skip)]
    fb: fbinit::FacebookInit,

    pub paths: InvocationPaths,

    /// This holds the main data shared across different commands.
    data: AsyncOnceCell<SharedResult<Arc<DaemonStateData>>>,

    dice_constructor: Box<dyn DaemonStateDiceConstructor>,
}

/// DaemonStateData is the main shared data across all commands. It's lazily initialized on
/// the first command that requires it.
#[derive(Allocative)]
pub struct DaemonStateData {
    /// The Dice computation graph. Generally, we shouldn't add things to the DaemonStateData
    /// (or DaemonState) itself and instead they should be represented on the computation graph.
    ///
    /// The DICE graph is held by the concurrency handler to manage locking for concurrent commands
    pub(crate) dice_manager: ConcurrencyHandler,

    /// Synced every time we run a command.
    file_watcher: Arc<dyn FileWatcher>,

    /// Settled every time we run a command.
    io: Arc<dyn IoProvider>,

    /// The RE connection, managed such that all build commands that are concurrently active uses
    /// the same connection. Once there are no active build commands, the connection will be
    /// terminated
    pub re_client_manager: Arc<ReConnectionManager>,

    /// Executor responsible for coordinating and rate limiting I/O.
    pub blocking_executor: Arc<dyn BlockingExecutor>,

    /// Most materializations go through the materializer, providing a single point
    /// where the most expensive network and fs IO operations are performed. It
    /// needs access to the `ReConnectionManager` to download from RE. It must
    /// live for the entire lifetime of the daemon, in order to allow deferred
    /// materializations to work properly between distinct build commands.
    materializer: Arc<dyn Materializer>,

    forkserver: Option<ForkserverClient>,

    /// Data pertaining to event logging, which controls the ways that event data is written throughout the course of
    /// a command.
    #[cfg_attr(not(fbcode_build), allow(dead_code))]
    event_logging_data: Arc<EventLoggingData>,

    /// Whether or not to hash all commands
    pub hash_all_commands: bool,

    pub start_time: Instant,

    #[allocative(skip)]
    pub create_unhashed_outputs_lock: Arc<Mutex<()>>,
}

impl DaemonStateData {
    pub fn dice_dump(&self, path: &Path, format: DiceDumpFormat) -> anyhow::Result<()> {
        crate::daemon::dice_dump::dice_dump(self.dice_manager.unsafe_dice(), path, format)
    }

    pub async fn spawn_dice_dump(&self, path: &Path, format: DiceDumpFormat) -> anyhow::Result<()> {
        crate::daemon::dice_dump::dice_dump_spawn(self.dice_manager.unsafe_dice(), path, format)
            .await
    }
}

impl DaemonStatePanicDiceDump for DaemonStateData {
    fn dice_dump(&self, path: &Path, format: DiceDumpFormat) -> anyhow::Result<()> {
        self.dice_dump(path, format)
    }
}

/// Configuration pertaining to event logging.
#[cfg_attr(not(fbcode_build), allow(dead_code))]
#[derive(Allocative)]
pub struct EventLoggingData {
    /// The size of the queue for in-flight messages.
    buffer_size: usize,
}

pub trait DaemonStateDiceConstructor: Allocative + Send + Sync + 'static {
    fn construct_dice(
        &self,
        io: Arc<dyn IoProvider>,
        root_config: &LegacyBuckConfig,
    ) -> anyhow::Result<Arc<Dice>>;
}

impl DaemonState {
    pub fn new(
        fb: fbinit::FacebookInit,
        paths: InvocationPaths,
        dice_constructor: Box<dyn DaemonStateDiceConstructor>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            fb,
            paths,
            data: AsyncOnceCell::new(),
            dice_constructor,
        })
    }

    // Creates the initial DaemonStateData.
    // Starts up the watchman query.
    async fn init_data(
        fb: fbinit::FacebookInit,
        paths: &InvocationPaths,
        dice_constructor: &dyn DaemonStateDiceConstructor,
    ) -> anyhow::Result<Arc<DaemonStateData>> {
        let fs = paths.project_root().clone();

        let legacy_cells = BuckConfigBasedCells::parse(&fs)?;
        let (legacy_configs, cells) = (legacy_cells.configs_by_name, legacy_cells.cell_resolver);

        let root_config = legacy_configs
            .get(cells.root_cell())
            .context("No config for root cell")?;

        // TODO(rafaelc): merge configs from all cells once they are consistent
        let static_metadata = Arc::new(RemoteExecutionStaticMetadata::from_legacy_config(
            root_config,
        )?);

        let ignore_specs: HashMap<CellName, IgnoreSet> = legacy_configs
            .iter()
            .map(|(cell, config)| {
                Ok((
                    cell.clone(),
                    IgnoreSet::from_ignore_spec(config.get("project", "ignore").unwrap_or(""))?,
                ))
            })
            .collect::<anyhow::Result<_>>()?;

        let materialization_method =
            MaterializationMethod::try_new_from_config(legacy_configs.get(cells.root_cell()).ok())?;
        let disk_state_options = DiskStateOptions::new(root_config, materialization_method.dupe())?;
        let blocking_executor = Arc::new(BuckBlockingExecutor::default_concurrency(fs.dupe())?);
        let cache_dir_path = paths.cache_dir_path();
        let valid_cache_dirs = paths.valid_cache_dirs();
        let fs_duped = fs.dupe();

        let (io, forkserver, _, (materializer_db, materializer_state)) =
            futures::future::try_join4(
                buck2_common::io::create_io_provider(
                    fb,
                    fs.dupe(),
                    legacy_configs.get(cells.root_cell()).ok(),
                ),
                maybe_launch_forkserver(root_config),
                (blocking_executor.dupe() as Arc<dyn BlockingExecutor>).execute_io_inline(|| {
                    // Using `execute_io_inline` is just out of convenience.
                    // It doesn't really matter what's used here since there's no IO-heavy
                    // operations on daemon startup
                    delete_unknown_disk_state(&cache_dir_path, &valid_cache_dirs, fs_duped)
                }),
                maybe_load_or_initialize_materializer_sqlite_db(
                    &disk_state_options,
                    paths,
                    blocking_executor.dupe() as Arc<dyn BlockingExecutor>,
                    root_config,
                    fs,
                ),
            )
            .await?;

        let re_client_manager = Arc::new(ReConnectionManager::new(
            fb,
            false,
            10,
            static_metadata,
            Some(paths.re_logs_dir().to_string()),
            paths.buck_out_dir().to_string(),
        ));
        let materializer = Self::create_materializer(
            fb,
            io.project_root().dupe(),
            paths.buck_out_dir(),
            re_client_manager.dupe(),
            blocking_executor.dupe(),
            materialization_method,
            root_config,
            materializer_db,
            materializer_state,
        )?;

        let buffer_size = root_config
            .parse("buck2", "event_log_buffer_size")?
            .unwrap_or(10000);
        let event_logging_data = Arc::new(EventLoggingData { buffer_size });

        let dice = dice_constructor.construct_dice(io.dupe(), root_config)?;

        // TODO(cjhopman): We want to use Expr::True here, but we need to workaround
        // https://github.com/facebook/watchman/issues/911. Adding other filetypes to
        // this list should be safe until we can revert it to Expr::True.

        let file_watcher = <dyn FileWatcher>::new(
            paths.project_root(),
            root_config,
            cells.dupe(),
            ignore_specs,
        )
        .context("Error creating a FileWatcher")?;

        let hash_all_commands = root_config
            .parse::<RolloutPercentage>("buck2", "hash_all_commands")?
            .unwrap_or_else(RolloutPercentage::never)
            .roll();

        let nested_invocation_config = root_config
            .parse::<NestedInvocation>("buck2", "nested_invocation")?
            .unwrap_or(NestedInvocation::Run);

        let parallel_invocation_config = root_config
            .parse::<ParallelInvocation>("buck2", "parallel_invocation")?
            .unwrap_or(ParallelInvocation::Run);

        let create_unhashed_outputs_lock = Arc::new(Mutex::new(()));

        // Kick off an initial sync eagerly. This gets Watchamn to start watching the path we care
        // about (potentially kicking off an initial crawl).

        // disable the eager spawn for watchman until we fix dice commit to avoid a panic TODO(bobyf)
        // tokio::task::spawn(watchman_query.sync());
        Ok(Arc::new(DaemonStateData {
            dice_manager: ConcurrencyHandler::new(
                dice,
                nested_invocation_config,
                parallel_invocation_config,
            ),
            file_watcher,
            io,
            re_client_manager,
            blocking_executor,
            materializer,
            forkserver,
            event_logging_data,
            hash_all_commands,
            start_time: std::time::Instant::now(),
            create_unhashed_outputs_lock,
        }))
    }

    fn create_materializer(
        fb: FacebookInit,
        fs: ProjectRoot,
        buck_out_path: ProjectRelativePathBuf,
        re_client_manager: Arc<ReConnectionManager>,
        blocking_executor: Arc<dyn BlockingExecutor>,
        materialization_method: MaterializationMethod,
        root_config: &LegacyBuckConfig,
        materializer_db: Option<MaterializerStateSqliteDb>,
        materializer_state: Option<MaterializerState>,
    ) -> anyhow::Result<Arc<dyn Materializer>> {
        match materialization_method {
            MaterializationMethod::Immediate => Ok(Arc::new(ImmediateMaterializer::new(
                fs,
                re_client_manager,
                blocking_executor,
            ))),
            MaterializationMethod::Deferred | MaterializationMethod::DeferredSkipFinalArtifacts => {
                let defer_write_actions = root_config
                    .parse("buck2", "defer_write_actions")?
                    .unwrap_or(false);

                // RE will refresh any TTL < 1 hour, so we check twice an hour and refresh any TTL
                // < 1 hour.
                let ttl_refresh_frequency = root_config
                    .parse("buck2", "ttl_refresh_frequency_seconds")?
                    .unwrap_or(1800);

                let ttl_refresh_min_ttl = root_config
                    .parse("buck2", "ttl_refresh_min_ttl_seconds")?
                    .unwrap_or(3600);

                let ttl_refresh_enabled = root_config
                    .parse::<RolloutPercentage>("buck2", "ttl_refresh_enabled")?
                    .unwrap_or_else(RolloutPercentage::never)
                    .roll();

                let config = DeferredMaterializerConfigs {
                    materialize_final_artifacts: matches!(
                        materialization_method,
                        MaterializationMethod::Deferred
                    ),
                    defer_write_actions,
                    ttl_refresh_frequency: std::time::Duration::from_secs(ttl_refresh_frequency),
                    ttl_refresh_min_ttl: chrono::Duration::seconds(ttl_refresh_min_ttl),
                    ttl_refresh_enabled,
                };

                Ok(Arc::new(DeferredMaterializer::new(
                    fs,
                    re_client_manager,
                    blocking_executor,
                    config,
                    materializer_db,
                    materializer_state,
                )))
            }
            MaterializationMethod::Eden => {
                #[cfg(any(fbcode_build, cargo_internal_build))]
                {
                    use buck2_execute::materialize::eden_api::EdenBuckOut;
                    use buck2_execute_impl::materializers::eden::EdenMaterializer;

                    let buck_out_mount = fs.root().join(&buck_out_path);

                    if cfg!(unix) {
                        Ok(Arc::new(
                            EdenMaterializer::new(
                                fs,
                                re_client_manager.dupe(),
                                blocking_executor,
                                EdenBuckOut::new(
                                    fb,
                                    buck_out_path,
                                    buck_out_mount,
                                    re_client_manager,
                                )
                                .context("Failed to create EdenFS-based buck-out")?,
                            )
                            .context("Failed to create Eden materializer")?,
                        ))
                    } else {
                        Err(anyhow::anyhow!(
                            "`eden` materialization method is not supported on Windows"
                        ))
                    }
                }
                #[cfg(not(any(fbcode_build, cargo_internal_build)))]
                {
                    let _unused = buck_out_path;
                    let _unused = fs;
                    let _unused = fb;
                    Err(anyhow::anyhow!(
                        "`eden` materialization method is only supported in Meta internal builds"
                    ))
                }
            }
        }
    }

    /// Prepares an event stream for a request by bootstrapping an event source and EventDispatcher pair. The given
    /// EventDispatcher will log to the returned EventSource and (optionally) to Scribe if enabled via buckconfig.
    pub async fn prepare_events(
        &self,
        trace_id: TraceId,
    ) -> SharedResult<(impl EventSource, EventDispatcher)> {
        // facebook only: logging events to Scribe.
        facebook_only();
        let (events, sink) = buck2_events::create_source_sink_pair();
        let data = self.data().await?;
        let dispatcher = if let Some(scribe_sink) =
            scribe::new_thrift_scribe_sink_if_enabled(self.fb, data.event_logging_data.buffer_size)?
        {
            EventDispatcher::new(trace_id, TeeSink::new(scribe_sink, sink))
        } else {
            // Writing to Scribe via the HTTP gateway (what we do for a Cargo build) is many times slower than the fbcode
            // Scribe client, so we don't do it. It's really, really bad for build performance - turning it on regresses
            // build performance by 10x.
            EventDispatcher::new(trace_id, sink)
        };
        Ok((events, dispatcher))
    }

    /// Prepares a ServerCommandContext for processing a complex command (that accesses the dice computation graph, for example).
    ///
    /// This initializes (if necessary) the shared daemon state and syncs the watchman query (to flush any recent filesystem events).
    pub async fn prepare_command(
        &self,
        dispatcher: EventDispatcher,
    ) -> SharedResult<BaseServerCommandContext> {
        check_working_dir::check_working_dir()?;

        let data = self.data().await?;

        let tags = vec![
            format!(
                "dice-detect-cycles:{}",
                data.dice_manager
                    .unsafe_dice()
                    .detect_cycles()
                    .variant_name()
            ),
            format!("hash-all-commands:{}", data.hash_all_commands),
        ];

        dispatcher.instant_event(buck2_data::TagEvent { tags });

        let drop_guard = ActiveCommandDropGuard::new(&dispatcher);

        // Sync any FS changes and invalidate DICE state if necessary.
        data.io.settle().await?;

        Ok(BaseServerCommandContext {
            _fb: self.fb,
            project_root: self.paths.project_root().clone(),
            dice_manager: data.dice_manager.dupe(),
            io: data.io.dupe(),
            re_client_manager: data.re_client_manager.dupe(),
            blocking_executor: data.blocking_executor.dupe(),
            materializer: data.materializer.dupe(),
            file_watcher: data.file_watcher.dupe(),
            events: dispatcher,
            forkserver: data.forkserver.dupe(),
            hash_all_commands: data.hash_all_commands,
            _drop_guard: drop_guard,
            daemon_start_time: data.start_time,
            create_unhashed_outputs_lock: data.create_unhashed_outputs_lock.dupe(),
        })
    }

    /// Initializes and returns the DaemonStateData, if it hasn't already been initialized already.
    pub async fn data(&self) -> SharedResult<Arc<DaemonStateData>> {
        self.data
            .get_or_init(async move {
                let result = Self::init_data(self.fb, &self.paths, &*self.dice_constructor)
                    .await
                    .context("Error initializing DaemonStateData");
                if let Ok(ref data) = result {
                    crate::daemon::panic::initialize(data.dupe());
                }
                result.shared_error()
            })
            .await
            .dupe()
    }
}
