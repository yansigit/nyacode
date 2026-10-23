use super::*;
impl ChatWidget {
    pub(crate) fn show_provider_manager(
        &mut self,
        handle: codex_app_server_client::AppServerRequestHandle,
    ) {
        self.bottom_pane
            .show_view(Box::new(crate::bottom_pane::ProviderManager::new(
                handle,
                self.thread_id.map(|id| id.to_string()),
            )));
    }
}
