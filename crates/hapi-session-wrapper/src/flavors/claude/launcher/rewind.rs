use super::Launcher;
use crate::flavors::claude::history::{self, Turn};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{io, time::Duration};
use tokio::time::{timeout, Instant};

#[derive(Clone, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Intent {
    target_local_id: String,
    truncate_from_local_id: String,
    keep_boundary: String,
    dropped_prompts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    discarded_through_seq: Option<u64>,
}

impl Intent {
    fn read(metadata: &Value) -> Option<Self> {
        serde_json::from_value(metadata["claudeRewind"].clone()).ok()
    }

    fn matches(&self, metadata: &Value) -> bool {
        Self::read(metadata).as_ref() == Some(self)
    }

    fn native_applied(&self, turns: &[Turn]) -> bool {
        !turns.is_empty()
            && turns
                .iter()
                .all(|turn| !self.dropped_prompts.contains(&turn.prompt))
    }
}

pub(super) enum Rewind {
    Idle,
    Unknown(Intent),
    AwaitingNative {
        intent: Intent,
        check_until: Option<Instant>,
    },
    AwaitingHub(Intent),
}

impl Rewind {
    pub(super) fn restore(metadata: &Value) -> Self {
        Intent::read(metadata).map_or(Self::Idle, Self::awaiting_native)
    }

    fn awaiting_native(intent: Intent) -> Self {
        Self::AwaitingNative {
            intent,
            check_until: None,
        }
    }

    pub(super) fn accepts_input(&self) -> bool {
        !matches!(self, Self::Unknown(_))
    }

    pub(super) fn blocks_history(&self, metadata: &Value) -> bool {
        !matches!(self, Self::Idle) || !metadata["claudeRewind"].is_null()
    }

    pub(super) fn needs_poll(&self) -> bool {
        match self {
            Self::Idle => false,
            Self::AwaitingNative { check_until, .. } => {
                check_until.is_some_and(|deadline| Instant::now() <= deadline)
            }
            Self::Unknown(_) | Self::AwaitingHub(_) => true,
        }
    }

    pub(super) fn check_native(&mut self) {
        if let Self::AwaitingNative { check_until, .. } = self {
            *check_until = Some(Instant::now() + Duration::from_secs(5));
        }
    }
}

pub(super) enum RewindLaunch {
    Unchanged,
    Restart { boundary: Option<String> },
}

impl RewindLaunch {
    pub(super) fn boundary(&self) -> Option<&str> {
        match self {
            Self::Unchanged => None,
            Self::Restart { boundary } => boundary.as_deref(),
        }
    }
}

impl Launcher {
    pub(super) fn prepare_rewind(&self, request: Value) -> Result<Intent, String> {
        let local_id = request["messageLocalId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or("messageLocalId is required")?;
        if !self.launch.supports_resume_at {
            return Err("Rewind is not supported for Claude".into());
        }
        let metadata = self.s.client.metadata.get().value;
        if self.rewind.blocks_history(&metadata) {
            return Err("A rewind is already pending; send a message to settle it first".into());
        }
        if !self.ready_for_input()
            || self.parked.is_some()
            || self.s.queue.lock().unwrap().size() != 0
        {
            return Err("Claude session is not idle".into());
        }
        let id = self.s.id.lock().unwrap().clone();
        if self.launch.fork_pending(id.as_deref()) {
            return Err("Send a message in this fork first, then rewind".into());
        }
        let id = id.as_deref().or_else(|| self.launch.resume_arg()).ok_or(
            "No active native session to rewind; send a message to start a new conversation first",
        )?;
        let turns = self.launch.native_turns(id);
        let index = history::prompt_index(&turns, &metadata, local_id)?;
        let previous = index
            .checked_sub(1)
            .ok_or("Cannot rewind the first message")?;
        Ok(Intent {
            target_local_id: local_id.into(),
            truncate_from_local_id: local_id.into(),
            keep_boundary: turns[previous].end.clone(),
            dropped_prompts: turns[index..]
                .iter()
                .map(|turn| turn.prompt.clone())
                .collect(),
            discarded_through_seq: request["discardedThroughSeq"]
                .as_u64()
                .filter(|seq| *seq <= 9_007_199_254_740_991),
        })
    }

    pub(super) async fn commit_rewind(&mut self, intent: Intent) -> Result<String, String> {
        let value = serde_json::to_value(&intent).map_err(|error| error.to_string())?;
        let client = &self.s.client;
        let write = client.metadata.update_cancellable(move |metadata| {
            let mut next = metadata.clone();
            if metadata["claudeRewind"].is_null() {
                next["claudeRewind"] = value.clone();
            }
            next
        });
        if !client.flush_metadata(Duration::from_secs(60)).await {
            write.cancel();
            self.rewind = Rewind::Unknown(intent);
            return Err("Rewind intent could not be persisted".into());
        }
        if !intent.matches(&client.metadata.get().value) {
            return Err("Rewind intent could not be persisted".into());
        }
        let local_id = intent.truncate_from_local_id.clone();
        self.rewind = Rewind::awaiting_native(intent);
        Ok(local_id)
    }

    fn rewind_turns(&self) -> Vec<Turn> {
        let id = self.s.id.lock().unwrap().clone();
        id.as_deref()
            .or_else(|| self.launch.resume_arg())
            .map(|id| self.launch.native_turns(id))
            .unwrap_or_default()
    }

    pub(super) fn plan_rewind_launch(&mut self) -> RewindLaunch {
        let Rewind::AwaitingNative { intent, .. } = &self.rewind else {
            return RewindLaunch::Unchanged;
        };
        let turns = self.rewind_turns();
        let launch = if turns.is_empty() {
            RewindLaunch::Restart { boundary: None }
        } else if intent.native_applied(&turns) {
            RewindLaunch::Unchanged
        } else {
            return RewindLaunch::Restart {
                boundary: Some(intent.keep_boundary.clone()),
            };
        };
        self.rewind = Rewind::AwaitingHub(intent.clone());
        launch
    }

    pub(super) async fn restart_rewind(&mut self) -> io::Result<()> {
        let RewindLaunch::Restart { boundary } = self.plan_rewind_launch() else {
            return Ok(());
        };
        if self.remote.is_none() && boundary.is_none() {
            return Ok(());
        }
        self.finish(false).await?;
        self.backoff = None;
        self.converter.reset_parent_chain();
        if let Some(boundary) = boundary.as_deref() {
            self.spawn(&self.startup_mode(), Some(boundary))?;
        }
        Ok(())
    }

    pub(super) async fn reconcile_rewind(&mut self) -> io::Result<()> {
        match &self.rewind {
            Rewind::Idle => {}
            Rewind::Unknown(intent) => {
                let client = &self.s.client;
                if !intent.matches(&client.metadata.get().value)
                    && !matches!(
                        timeout(Duration::from_secs(2), client.refresh_metadata()).await,
                        Ok(true)
                    )
                {
                    return Ok(());
                }
                if let Some(committed) =
                    Intent::read(&client.metadata.get().value).filter(|found| found == intent)
                {
                    self.rewind = Rewind::awaiting_native(committed);
                    self.restart_rewind().await?;
                } else {
                    self.rewind = Rewind::Idle;
                }
            }
            Rewind::AwaitingNative { intent, .. } => {
                if intent.native_applied(&self.rewind_turns()) {
                    self.rewind = Rewind::AwaitingHub(intent.clone());
                }
            }
            Rewind::AwaitingHub(intent) => {
                let client = &self.s.client;
                let metadata = client.metadata.get().value;
                if !intent.matches(&metadata) {
                    self.rewind = Rewind::Idle;
                } else if metadata["claudeRewind"]["hubCutApplied"] == true {
                    let write = client
                        .metadata
                        .update_cancellable(Self::remove_intent(intent.clone(), true));
                    if !client.flush_metadata(Duration::from_secs(2)).await {
                        write.cancel();
                    } else if !intent.matches(&client.metadata.get().value) {
                        self.rewind = Rewind::Idle;
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn cancel_rewind(&mut self) {
        self.rewind = Rewind::Idle;
        if let Some(intent) = Intent::read(&self.s.client.metadata.get().value) {
            self.s
                .client
                .metadata
                .update(Self::remove_intent(intent, false));
        }
    }

    fn remove_intent(intent: Intent, require_hub_cut: bool) -> impl Fn(&Value) -> Value {
        move |metadata| {
            let mut next = metadata.clone();
            if intent.matches(metadata)
                && (!require_hub_cut || metadata["claudeRewind"]["hubCutApplied"] == true)
            {
                next.as_object_mut().unwrap().shift_remove("claudeRewind");
            }
            next
        }
    }
}
