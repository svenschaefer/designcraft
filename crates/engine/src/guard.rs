//! Last-resort guard (craftrules `standards/never-crash.md`): a panic that escapes a command —
//! any edit, import or export, from the UI, the CLI, MCP or the control channel — becomes an
//! error, and the document is kept as it was before the command (and saved for crash recovery
//! when a recovery directory is set). It is a safety net: commands still must not panic.

use std::panic::{AssertUnwindSafe, catch_unwind};

use serde_json::Value;

use crate::{DocState, EngineError, Result, Session};

/// Document state and command bookkeeping commit together, including nested commands.
struct CommandSnapshot {
    docs: Vec<DocState>,
    active: Option<usize>,
    journal_len: usize,
    requests_len: usize,
    transforms: (Vec<(String, Value)>, Vec<designcraft_doc::ItemId>, bool),
    loaded: Option<(designcraft_doc::AssetId, (f64, f64))>,
    untitled: u32,
}

impl CommandSnapshot {
    fn new(session: &Session) -> Self {
        Self {
            docs: session.docs.clone(),
            active: session.active,
            journal_len: session.journal.len(),
            requests_len: session.ui_requests.len(),
            transforms: session.transforms.clone(),
            loaded: session.loaded,
            untitled: session.untitled,
        }
    }

    fn restore(mut self, session: &mut Session) {
        // Invalidate views of changed documents while keeping the saved/dirty and history state.
        for before in &mut self.docs {
            if let Some(current) = session.docs.iter().find(|d| d.uid == before.uid)
                && (current.revision != before.revision || !std::sync::Arc::ptr_eq(&current.doc, &before.doc))
            {
                before.revision = before.revision.max(current.revision).saturating_add(1);
            }
        }
        session.docs = self.docs;
        session.active = self.active;
        session.journal.truncate(self.journal_len);
        session.ui_requests.truncate(self.requests_len);
        session.transforms = self.transforms;
        session.loaded = self.loaded;
        session.untitled = self.untitled;
    }
}

/// Text of a panic payload (`panic!` with a literal or a formatted message).
pub fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

/// Log every panic (message and location) before it unwinds, then run the previous hook.
/// Installed once per process, when the first [`Session`] is made.
pub fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let at = info.location().map(|l| format!(" at {}:{}", l.file(), l.line())).unwrap_or_default();
            log::error!("panic{at}: {}", panic_message(info.payload()));
            previous(info);
        }));
    });
}

impl Session {
    /// Run command `id` (`f`) inside the guard.
    pub(crate) fn guarded(&mut self, id: &str, f: impl FnOnce(&mut Session) -> Result<Value>) -> Result<Value> {
        let before = CommandSnapshot::new(self);
        match catch_unwind(AssertUnwindSafe(|| f(self))) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => {
                before.restore(self);
                Err(error)
            }
            Err(payload) => {
                let msg = panic_message(payload.as_ref());
                before.restore(self);
                if let Some(dir) = self.recovery_dir.clone()
                    && let Err(e) = crate::recovery::save(self, &dir)
                {
                    log::error!("crash recovery save after `{id}` failed: {e}");
                }
                log::error!("command `{id}` panicked: {msg}");
                Err(EngineError::Internal(id.to_string(), msg))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::Session;

    #[test]
    fn a_panicking_command_is_an_error_and_keeps_the_document() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        s.execute("frame.create", &json!({"rect": [36, 36, 200, 200], "content": "text", "text": "keep me"})).unwrap();
        let before = s.active().unwrap().doc.clone();
        // A command that edits the document and then panics half way.
        let err = s
            .guarded("test.panic", |s| {
                s.execute("frame.create", &json!({"rect": [36, 236, 200, 400]}))?;
                panic!("boom");
            })
            .unwrap_err();
        assert!(err.to_string().contains("test.panic") && err.to_string().contains("boom"), "{err}");
        let st = s.active().unwrap();
        assert!(std::sync::Arc::ptr_eq(&st.doc, &before), "the document is the one from before the command");
        // The session keeps working.
        s.execute("frame.create", &json!({"rect": [36, 236, 200, 400]})).unwrap();
    }

    fn session_with_redo() -> Session {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        s.execute("frame.create", &json!({"rect": [36, 36, 200, 200], "content": "text", "text": "keep me"})).unwrap();
        s.execute("frame.create", &json!({"rect": [36, 236, 200, 400]})).unwrap();
        s.execute("edit.undo", &json!({})).unwrap();
        s
    }

    fn assert_history_kept(s: &Session, before: &crate::DocState) {
        let after = s.active().unwrap();
        assert!(std::sync::Arc::ptr_eq(&before.doc, &after.doc));
        assert!(std::sync::Arc::ptr_eq(&before.saved_doc, &after.saved_doc));
        assert_eq!(after.is_dirty(), before.is_dirty());
        for (old, new) in [(&before.history.undo, &after.history.undo), (&before.history.redo, &after.history.redo)] {
            assert_eq!(old.len(), new.len());
            for (a, b) in old.iter().zip(new) {
                assert_eq!(a.label, b.label);
                assert!(std::sync::Arc::ptr_eq(&a.doc, &b.doc));
            }
        }
    }

    #[test]
    fn panic_after_two_nested_edits_keeps_undo_redo_and_journal() {
        let mut s = session_with_redo();
        let before = s.active().unwrap().clone();
        let journal = s.journal.clone();
        let undo_target = before.history.undo.last().unwrap().doc.clone();
        let error = s
            .guarded("test.two-edits", |s| {
                for y in [236, 436] {
                    s.execute("frame.create", &json!({"rect": [36, y, 200, y + 164]}))?;
                }
                panic!("after nested edits");
            })
            .unwrap_err();
        assert!(matches!(error, crate::EngineError::Internal(_, _)));
        assert_history_kept(&s, &before);
        assert_eq!(s.journal, journal);
        s.execute("edit.undo", &json!({})).unwrap();
        assert!(std::sync::Arc::ptr_eq(&s.active().unwrap().doc, &undo_target));
        s.execute("edit.redo", &json!({})).unwrap();
        assert!(std::sync::Arc::ptr_eq(&s.active().unwrap().doc, &before.doc));
    }

    #[test]
    fn ordinary_failure_after_nested_edits_also_rolls_back() {
        let mut s = session_with_redo();
        let before = s.active().unwrap().clone();
        let journal = s.journal.clone();
        assert!(
            s.guarded("test.error", |s| {
                s.execute("frame.create", &json!({"rect": [0, 0, 100, 100]}))?;
                Err(crate::EngineError::Other("reject compound edit".into()))
            })
            .is_err()
        );
        assert_history_kept(&s, &before);
        assert_eq!(s.journal, journal);
    }

    #[test]
    fn panic_after_a_document_switch_restores_the_document_list_and_active_document() {
        let mut s = session_with_redo();
        let before = s.active().unwrap().clone();
        let active = s.active;
        let count = s.docs.len();
        let untitled = s.untitled;
        let _ = s.guarded("test.switch", |s| {
            s.execute("file.new", &json!({}))?;
            s.docs.remove(0);
            s.active = Some(0);
            panic!("after switching documents");
        });
        assert_eq!(s.docs.len(), count);
        assert_eq!(s.active, active);
        assert_eq!(s.untitled, untitled);
        assert_history_kept(&s, &before);
    }
}
