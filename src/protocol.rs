use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Executor,
    Planner,
    Verifier,
    Diagnostics,
    Solo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageType {
    Handoff,
    Result,
    Verification,
    Failure,
    Blocker,
    Plan,
    Diagnostics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageStatus {
    Complete,
    InProgress,
    Failed,
    Verified,
    Ready,
    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info_renamed,
    Warn,
    Error,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockerPayload {
    pub summary: String,
    pub blocker: String,
    pub evidence: String,
    pub required_action: String,
    #[serde(default)]
    pub severity: Option<Severity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessagePayload {
    Blocker(BlockerPayload),
    Generic(serde_json::Value),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolMessage {
    pub action: ActionKind,
    #[serde(alias = "from_role")]
    pub from: Role,
    #[serde(alias = "to_role")]
    pub to: Role,
    #[serde(rename = "type")]
    pub msg_type: MessageType,
    pub status: MessageStatus,
    pub observation: String,
    pub rationale: String,
    pub payload: MessagePayload,
    #[serde(default)]
    pub severity: Option<Severity>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Message,
    ListDir,
    ReadFile,
    ApplyPatch,
    RunCommand,
    Python,
    CargoTest,
    Plan,
    RustcHir,
    RustcMir,
    GraphProbe,
    GraphCall,
    GraphCfg,
    GraphDataflow,
    GraphReachability,
}
