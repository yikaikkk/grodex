use serde::{Deserialize, Serialize};

use crate::runtime::PreparedOperation;

#[derive(Serialize, Deserialize, Debug)]
pub enum SupervisorRequest {
    PrepareOperation { operation: PreparedOperation },
    EnforceSeatbelt { profile: String, executable: String },
    ExecuteChild {
        program: String,
        args: Vec<String>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
    },
    HealthCheck,
    Shutdown,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum SupervisorResponse {
    Accepted { operation_id: String },
    Refused { reason: String },
    ExecuteResult {
        exit_code: i32,
        stdout_b64: String,
        stderr_b64: String,
    },
    Pong,
    Error { code: String, message: String },
}
