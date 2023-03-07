#![warn(missing_docs, unreachable_pub)]
#![deny(unused_must_use, rust_2018_idioms)]
#![doc(test(
    no_crate_inject,
    attr(deny(warnings, rust_2018_idioms), allow(dead_code, unused_variables))
))]

//! sync controller

use futures::{Future, FutureExt, StreamExt};
use reth_db::database::Database;
use reth_interfaces::{consensus::ForkchoiceState, sync::SyncStateUpdater};
use reth_primitives::SealedBlock;
use reth_stages::{Pipeline, PipelineFut};
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio_stream::wrappers::UnboundedReceiverStream;

enum PipelineState<DB: Database, U: SyncStateUpdater> {
    Idle(Pipeline<DB, U>),
    Running(PipelineFut<DB, U>),
}

// TODO:
#[allow(dead_code)]
enum SyncControllerMessage {
    ForkchoiceUpdated(ForkchoiceState),
    NewPayload(SealedBlock),
}

struct SyncController<DB: Database, U: SyncStateUpdater> {
    db: Arc<DB>,
    message_rx: UnboundedReceiverStream<SyncControllerMessage>,
    forkchoice_state: Option<ForkchoiceState>,
    pipeline_state: Option<PipelineState<DB, U>>,
    // blockchain_tree: BlockchainTree<DB, C>,
}

impl<DB, U> SyncController<DB, U>
where
    DB: Database + Unpin + 'static,
    U: SyncStateUpdater + Unpin + 'static,
{
    fn next_pipeline_state(
        &mut self,
        cx: &mut Context<'_>,
        current: PipelineState<DB, U>,
        sync_needed: bool,
    ) -> PipelineState<DB, U> {
        match current {
            PipelineState::Running(mut fut) => {
                match fut.poll_unpin(cx) {
                    Poll::Ready((pipeline, _result)) => {
                        // TODO: handle result
                        if sync_needed {
                            PipelineState::Running(pipeline.run_as_fut(self.db.clone()))
                        } else {
                            PipelineState::Idle(pipeline)
                        }
                    }
                    Poll::Pending => PipelineState::Running(fut),
                }
            }
            PipelineState::Idle(pipeline) => {
                if sync_needed {
                    PipelineState::Running(pipeline.run_as_fut(self.db.clone()))
                } else {
                    PipelineState::Idle(pipeline)
                }
            }
        }
    }
}

impl<DB, U> Future for SyncController<DB, U>
where
    DB: Database + Unpin + 'static,
    U: SyncStateUpdater + Unpin + 'static,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        let pipeline_sync_needed = false;
        while let Poll::Ready(Some(msg)) = this.message_rx.poll_next_unpin(cx) {
            match msg {
                SyncControllerMessage::ForkchoiceUpdated(state) => {
                    // TODO:
                    // this.blockchain_tree.make_canonical(state.head_block_hash);
                    this.forkchoice_state = Some(state);
                }
                SyncControllerMessage::NewPayload(_block) => {
                    // TODO:
                    // if pipeline_sync_is_close
                    // this.blockchain_tree.insert_block(block);
                }
            }
        }

        let _forckchoice_state = match &this.forkchoice_state {
            Some(state) => state,
            None => return Poll::Pending,
        };

        // TODO:
        let current_pipeline_state = this.pipeline_state.take().expect("pipeline state is set");
        let next_pipeline_state =
            this.next_pipeline_state(cx, current_pipeline_state, pipeline_sync_needed);
        this.pipeline_state = Some(next_pipeline_state);

        Poll::Pending
    }
}
