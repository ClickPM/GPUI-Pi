pub use pi_runtime::*;

pub trait ExtensionResponseSender: Send + Sync {
    fn send(&self, response: pi_rpc::ExtensionUiResponse) -> Result<(), String>;
}

#[derive(Clone)]
pub struct HandleExtensionResponseSender {
    handle: SessionHandle,
    epoch: u64,
}

impl HandleExtensionResponseSender {
    pub fn new(handle: SessionHandle, epoch: u64) -> Self {
        Self { handle, epoch }
    }
}

impl ExtensionResponseSender for HandleExtensionResponseSender {
    fn send(&self, response: pi_rpc::ExtensionUiResponse) -> Result<(), String> {
        self.handle.respond_extension_ui(self.epoch, response)
    }
}
