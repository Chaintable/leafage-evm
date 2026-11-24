#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum VmHook {
    AccountValidationEntered,
    PaymasterValidationEntered,
    ValidationExited,
    ValidationStepEnded,
    TxHasEnded,
    DebugLog,
    DebugReturnData,
    NearCallCatch,
    AskOperatorForRefund,
    NotifyAboutRefund,
    PostResult,
    FinalBatchInfo,
    PubdataRequested,
    Unknown(u32),
}

impl VmHook {
    pub fn from_raw(raw: u32) -> Self {
        match raw {
            0 => Self::AccountValidationEntered,
            1 => Self::PaymasterValidationEntered,
            2 => Self::ValidationExited,
            3 => Self::ValidationStepEnded,
            4 => Self::TxHasEnded,
            5 => Self::DebugLog,
            6 => Self::DebugReturnData,
            7 => Self::NearCallCatch,
            8 => Self::AskOperatorForRefund,
            9 => Self::NotifyAboutRefund,
            10 => Self::PostResult,
            11 => Self::FinalBatchInfo,
            12 => Self::PubdataRequested,
            other => Self::Unknown(other),
        }
    }
}
