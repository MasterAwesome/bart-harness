use std::path::PathBuf;

use reqwest::Client;

use super::{Session, Settings};
use crate::{
    auth::Store,
    mcp,
    preferences::Current,
    record::{Resumed, Sessions},
    tool::{self, Context},
};

#[derive(Clone)]
pub(crate) struct Opener {
    pub(crate) sessions: Sessions,
    pub(crate) cwd: PathBuf,
    pub(crate) executable: PathBuf,
    pub(crate) client: Client,
    pub(crate) store: Store,
}

impl Opener {
    pub(crate) async fn open(
        &self,
        settings: Settings,
        preferences: Current,
        resumed: Option<Resumed>,
    ) -> Result<(Session, String), String> {
        let meta = resumed.as_ref().map(|_| settings.meta(&self.cwd));
        let opener = self.clone();
        let (mut session, id, conversation, name) =
            tokio::task::spawn_blocking(move || opener.construct(settings, preferences, resumed))
                .await
                .map_err(|error| format!("session opening worker failed: {error}"))??;
        session.resume(meta.as_ref(), name, conversation).await;
        Ok((session, id))
    }

    fn construct(
        &self,
        settings: Settings,
        preferences: Current,
        resumed: Option<Resumed>,
    ) -> Result<
        (
            Session,
            String,
            crate::conversation::Conversation,
            Option<String>,
        ),
        String,
    > {
        let Self {
            sessions,
            cwd,
            executable,
            client,
            store,
        } = self;
        let confinement = preferences.effective.permissions.confinement()?;
        let (id, record, conversation, name) = if let Some(Resumed {
            id,
            record,
            conversation,
            name,
            ..
        }) = resumed
        {
            (id, record, conversation, name)
        } else {
            let (id, record) = sessions
                .create(&settings.meta(cwd), &super::prompt::project(cwd))
                .map_err(|error| format!("cannot create a session record: {error}"))?;
            (
                id,
                record,
                crate::conversation::Conversation::default(),
                None,
            )
        };
        let spill_dir = sessions.dir(&id).join("output");
        let mcp = mcp::Registry::start(
            &mcp::Config::load(&mcp::path(store.config_dir()))?,
            tool::Place {
                cwd: cwd.clone(),
                executable: executable.clone(),
                spill_dir: spill_dir.clone(),
                confinement,
            },
            client,
            store,
        );
        let context = Context::new(
            cwd.clone(),
            executable.clone(),
            spill_dir,
            tool::OUTPUT_BUDGET_BYTES,
            confinement,
            mcp,
        )
        .map_err(|error| format!("cannot open saved outputs: {error}"))?;
        let session = Session::new(
            client.clone(),
            store.clone(),
            settings,
            preferences,
            context,
            id.clone(),
            Some(record),
        );
        Ok((session, id, conversation, name))
    }
}
