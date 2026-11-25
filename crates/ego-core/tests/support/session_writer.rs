//! Serialized writer authority for in-memory SessionStore contract fixtures.

use std::collections::BTreeSet;
use std::sync::Mutex;

use ego_core::event::domain::DomainEvent;
use ego_core::event::id::SessionId;
use ego_core::run::{RunAppend, RunRevision, RunTransaction};
use ego_core::state::SessionState;
use ego_core::store::session_store::{
    SessionEntry, SessionMeta, SessionStore, SessionWriterLease, StoreError,
};
use futures::stream::BoxStream;
use futures::{FutureExt, TryStreamExt};

pub fn open<'a>(
    store: &'a dyn SessionStore,
    writers: &'a Mutex<BTreeSet<SessionId>>,
    session: SessionId,
) -> Result<SessionWriterLease<'a>, StoreError> {
    if !writers.lock().expect("fixture writers").insert(session) {
        return Err(StoreError::Io(std::io::Error::other("writer occupied")));
    }
    let writer = Writer {
        store,
        writers,
        session,
    };
    let entries: Vec<SessionEntry> = writer
        .read(&session)?
        .try_collect()
        .now_or_never()
        .expect("in-memory fixture read is ready")?;
    let mut state = SessionState::default();
    for entry in entries {
        if let SessionEntry::Event(event) = entry {
            state.apply(&event);
        }
    }
    Ok(SessionWriterLease {
        state,
        writer: Box::new(writer),
    })
}

struct Writer<'a> {
    store: &'a dyn SessionStore,
    writers: &'a Mutex<BTreeSet<SessionId>>,
    session: SessionId,
}

impl Drop for Writer<'_> {
    fn drop(&mut self) {
        self.writers
            .lock()
            .expect("fixture writers")
            .remove(&self.session);
    }
}

impl SessionStore for Writer<'_> {
    fn append(&self, id: &SessionId, event: &DomainEvent) -> Result<(), StoreError> {
        assert_eq!(*id, self.session);
        self.store.append(id, event)
    }
    fn read(
        &self,
        id: &SessionId,
    ) -> Result<BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError> {
        assert_eq!(*id, self.session);
        self.store.read(id)
    }
    fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        self.store.list()
    }
    fn compare_and_append_run(
        &self,
        id: &SessionId,
        revision: Option<RunRevision>,
        transaction: RunTransaction,
    ) -> Result<RunAppend, StoreError> {
        assert_eq!(*id, self.session);
        self.store.compare_and_append_run(id, revision, transaction)
    }
}
