//! Folded into `tray.rs`: the clipboard listener lives on the tray's message-only
//! window (`Tray::set_clipboard_listening` / `Tray::clipboard_text`), so one hidden
//! window and one message thread serve both. This module keeps the C# file's
//! name findable; it has no code of its own.
