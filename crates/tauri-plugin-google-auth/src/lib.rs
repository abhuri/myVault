use tauri::{
    plugin::{Builder, TauriPlugin},
    Manager, Runtime,
};

mod error;
mod mobile;
mod models;

pub use error::{Error, Result};
pub use models::{AccessToken, Authorization};

pub const GOOGLE_DRIVE_SCOPE: &str = "https://www.googleapis.com/auth/drive";
const GOOGLE_DRIVE_METADATA_READONLY_SCOPE: &str =
    "https://www.googleapis.com/auth/drive.metadata.readonly";

use mobile::GoogleAuth;

/// Native-only access to Google authorization. The handle is deliberately not
/// exposed through a JavaScript plugin command or capability.
pub trait GoogleAuthExt<R: Runtime> {
    fn google_auth(&self) -> &GoogleAuth<R>;
}

impl<R: Runtime, T: Manager<R>> GoogleAuthExt<R> for T {
    fn google_auth(&self) -> &GoogleAuth<R> {
        self.state::<GoogleAuth<R>>().inner()
    }
}

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("google-auth")
        .setup(|app, api| {
            app.manage(mobile::init(app, api)?);
            Ok(())
        })
        .build()
}
