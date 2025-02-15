/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::collections::HashMap;
use std::future::Future;

use async_trait::async_trait;
use buck2_common::result::SharedResult;
use buck2_core::fs::project::ProjectRelativePath;
use buck2_core::fs::project::ProjectRoot;
use buck2_core::pattern::ParsedPattern;
use buck2_core::pattern::ProvidersPattern;
use buck2_data::CommandCriticalEnd;
use buck2_data::CommandCriticalStart;
use buck2_events::dispatch::EventDispatcher;
use dice::DiceComputations;
use dice::DiceTransaction;
use gazebo::prelude::*;

use crate::concurrency::ConcurrencyHandler;
use crate::concurrency::DiceDataProvider;
use crate::concurrency::DiceUpdater;
use crate::raw_output::RawOuputGuard;

#[async_trait]
pub trait ServerCommandContextTrait: Send + Sync + 'static {
    fn working_dir(&self) -> &ProjectRelativePath;

    fn project_root(&self) -> &ProjectRoot;

    /// exposes the dice for scoped access, but isn't intended to be callable by anyone
    async fn dice_accessor(&self, private: PrivateStruct) -> SharedResult<DiceAccessor>;

    fn events(&self) -> &EventDispatcher;

    fn stdout(&self) -> anyhow::Result<RawOuputGuard<'_>>;

    fn stderr(&self) -> anyhow::Result<RawOuputGuard<'_>>;

    async fn request_metadata(&self) -> anyhow::Result<HashMap<String, String>>;

    async fn config_metadata(
        &self,
        ctx: &DiceComputations,
    ) -> anyhow::Result<HashMap<String, String>>;

    fn log_target_pattern(&self, providers_patterns: &[ParsedPattern<ProvidersPattern>]);
}

pub struct PrivateStruct(());

pub struct DiceAccessor {
    pub dice_handler: ConcurrencyHandler,
    pub data: Box<dyn DiceDataProvider>,
    pub setup: Box<dyn DiceUpdater>,
    pub is_nested_invocation: bool,
    pub sanitized_argv: Vec<String>,
}

#[async_trait]
pub trait ServerCommandDiceContext {
    async fn with_dice_ctx<'v, F, Fut, R>(&'v self, exec: F) -> anyhow::Result<R>
    where
        F: FnOnce(&'v Box<dyn ServerCommandContextTrait>, DiceTransaction) -> Fut + Send,
        Fut: Future<Output = anyhow::Result<R>> + Send;
}

#[async_trait]
impl ServerCommandDiceContext for Box<dyn ServerCommandContextTrait> {
    /// Allows running a section of code that uses the shared DiceTransaction
    async fn with_dice_ctx<'v, F, Fut, R>(&'v self, exec: F) -> anyhow::Result<R>
    where
        F: FnOnce(&'v Box<dyn ServerCommandContextTrait>, DiceTransaction) -> Fut + Send,
        Fut: Future<Output = anyhow::Result<R>> + Send,
    {
        let dice_accessor = self.dice_accessor(PrivateStruct(())).await?;

        dice_accessor
            .dice_handler
            .enter(
                self.events().trace_id().dupe(),
                dice_accessor.data,
                &*dice_accessor.setup,
                |dice| async move {
                    let events = self.events().dupe();

                    let metadata = self.config_metadata(&dice).await?;

                    events
                        .span_async(
                            CommandCriticalStart {
                                metadata: metadata.clone(),
                                dice_version: dice.version(),
                            },
                            async move { (exec(self, dice).await, CommandCriticalEnd { metadata }) },
                        )
                        .await
                },
                dice_accessor.is_nested_invocation,
                dice_accessor.sanitized_argv,
            )
            .await?
    }
}
