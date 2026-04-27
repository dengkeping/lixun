use lixun_sources::source::SourceContext;
use std::sync::Arc;
use tokio::task::JoinHandle;

use crate::registry::{SourceInstance, SourceRegistry};
use crate::writer_sink::WriterSink;

pub fn spawn_all(registry: &SourceRegistry, sink: Arc<WriterSink>) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();
    for inst in &registry.instances {
        if let Some(interval) = inst.source.tick_interval() {
            let inst_clone = clone_instance(inst);
            let sink_clone = sink.clone();
            handles.push(tokio::spawn(async move {
                run_one(inst_clone, interval, sink_clone).await
            }));
        }
    }
    handles
}

fn clone_instance(inst: &SourceInstance) -> SourceInstance {
    SourceInstance {
        instance_id: inst.instance_id.clone(),
        state_dir: inst.state_dir.clone(),
        source: inst.source.clone(),
    }
}

async fn run_one(inst: SourceInstance, interval: std::time::Duration, sink: Arc<WriterSink>) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let ctx_instance = inst.instance_id.clone();
        let ctx_state = inst.state_dir.clone();
        let src = inst.source.clone();
        let sink_for_task = sink.clone();
        let result = tokio::task::spawn_blocking(move || {
            let ctx = SourceContext {
                instance_id: &ctx_instance,
                state_dir: &ctx_state,
            };
            src.on_tick(&ctx, sink_for_task.as_ref())
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(
                "tick for source {} (instance {}) failed: {}",
                inst.source.kind(),
                inst.instance_id,
                e
            ),
            Err(e) => tracing::error!("tick task for {} panicked: {}", inst.source.kind(), e),
        }
    }
}
