use std::path::PathBuf;

use reqwest::Client;

use super::{Session, Settings, journal::Restored};
use crate::{
    auth::Store,
    mcp,
    preferences::Current,
    record::{Resumed, Sessions},
    tool::{self, Context},
};

#[derive(Clone)]
pub struct Opener {
    pub sessions: Sessions,
    pub cwd: PathBuf,
    pub executable: PathBuf,
    pub client: Client,
    pub store: Store,
}

impl Opener {
    pub async fn open(
        &self,
        settings: Settings,
        preferences: Current,
        resumed: Option<Resumed>,
    ) -> Result<(Session, String), String> {
        let meta = resumed.as_ref().map(|_| settings.meta(&self.cwd));
        let opener = self.clone();
        let (mut session, id, restored) =
            tokio::task::spawn_blocking(move || opener.construct(settings, preferences, resumed))
                .await
                .map_err(|error| format!("session opening worker failed: {error}"))??;
        session.resume(meta.as_ref(), restored).await;
        Ok((session, id))
    }

    fn construct(
        &self,
        settings: Settings,
        preferences: Current,
        resumed: Option<Resumed>,
    ) -> Result<(Session, String, Restored), String> {
        let Self {
            sessions,
            cwd,
            executable,
            client,
            store,
        } = self;
        let confinement = preferences.effective.permissions.confinement()?;
        let fresh = resumed.is_none();
        let (id, record, restored) = if let Some(Resumed {
            id,
            record,
            conversation,
            name,
            prompt,
            ..
        }) = resumed
        {
            (
                id,
                record,
                Restored {
                    conversation,
                    name,
                    prompt,
                },
            )
        } else {
            let (id, record) = sessions
                .create(&settings.meta(cwd), &super::prompt::project(cwd))
                .map_err(|error| format!("cannot create a session record: {error}"))?;
            (id, record, Restored::default())
        };
        let place = tool::Place {
            cwd: cwd.clone(),
            executable: executable.clone(),
            spill_dir: sessions.dir(&id).join("output"),
            confinement,
        };
        let setup = mcp::Config::load(&mcp::path(store.config_dir())).and_then(|config| {
            Context::open_at(place, tool::OUTPUT_BUDGET_BYTES, |place| {
                mcp::Registry::start(&config, place.clone(), client, store)
            })
            .map_err(|error| format!("cannot open saved outputs: {error}"))
        });
        let context = match setup {
            Ok(context) => context,
            Err(error) => {
                drop(record);
                if fresh && let Err(cleanup) = sessions.delete_if_empty(&id) {
                    return Err(format!("{error}; cannot clean up session {id}: {cleanup}"));
                }
                return Err(error);
            }
        };
        let session = Session::new(
            client.clone(),
            store.clone(),
            settings,
            preferences,
            context,
            id.clone(),
            Some(record),
        );
        Ok((session, id, restored))
    }
}
