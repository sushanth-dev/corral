//! Clipboard access behind a trait so tests inject a memory
//! implementation and never touch the real system clipboard.

/// Set-text sink. `SystemClipboard` wraps arboard; tests use
/// `MemoryClipboard`.
pub trait Clipboard: std::fmt::Debug {
    fn set_text(&mut self, text: &str) -> anyhow::Result<()>;
}

#[derive(Default)]
pub struct SystemClipboard {
    inner: Option<arboard::Clipboard>,
}

impl std::fmt::Debug for SystemClipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemClipboard")
            .field("open", &self.inner.is_some())
            .finish()
    }
}

impl SystemClipboard {
    pub fn new() -> Self {
        Self::default()
    }

    fn handle(&mut self) -> anyhow::Result<&mut arboard::Clipboard> {
        if self.inner.is_none() {
            self.inner = Some(arboard::Clipboard::new()?);
        }
        Ok(self.inner.as_mut().expect("just initialized"))
    }
}

impl Clipboard for SystemClipboard {
    fn set_text(&mut self, text: &str) -> anyhow::Result<()> {
        self.handle()?.set_text(text.to_string())?;
        Ok(())
    }
}

// Test double; only referenced from test builds and the trait-object
// documentation test, so allow the non-test dead-code warning.
#[allow(dead_code)]
#[derive(Debug, Default)]
pub struct MemoryClipboard {
    pub text: String,
}

impl Clipboard for MemoryClipboard {
    fn set_text(&mut self, text: &str) -> anyhow::Result<()> {
        self.text = text.to_string();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_clipboard_stores_yanked_text() {
        let mut cb = MemoryClipboard::default();
        cb.set_text("hello\nworld").unwrap();
        assert_eq!(cb.text, "hello\nworld");
    }

    #[test]
    fn memory_clipboard_overwrites_previous_yank() {
        let mut cb = MemoryClipboard::default();
        cb.set_text("first").unwrap();
        cb.set_text("second").unwrap();
        assert_eq!(cb.text, "second");
    }

    #[test]
    fn clipboard_trait_object_works_behind_the_box() {
        // The wrapper exists so client code can hold Box<dyn Clipboard>.
        let mut memory: Box<dyn Clipboard> = Box::new(MemoryClipboard::default());
        memory.set_text("x").unwrap();
        assert!(format!("{memory:?}").contains("MemoryClipboard"));
    }
}
