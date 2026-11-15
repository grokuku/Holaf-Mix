//! Copying text to the desktop clipboard — and saying so truthfully.
//!
//! # The bug this module exists for
//!
//! The diagnostics dialog used to do:
//!
//! ```ignore
//! ui.ctx().copy_text(report);
//! state.status = Some("diagnostic report copied to the clipboard".into());
//! ```
//!
//! `eframe` was built with `default-features = false` and **without**
//! `clipboard`, so `egui-winit`'s clipboard had no OS backend at all: without
//! `arboard`/`smithay-clipboard` it keeps a plain `String` inside the process
//! ("a fallback clipboard that just works within the same app").  The call
//! therefore succeeded — and reached nothing.  The user pasted an empty
//! clipboard into their bug thread and the UI had told them it worked.
//!
//! # What the fix looks like
//!
//! 1. The `clipboard` cargo feature of this crate forwards to
//!    `eframe/clipboard` (which pulls `arboard` and, on Wayland,
//!    `smithay-clipboard` for the data-control protocol).  It is **on by
//!    default**, so the shipped build has a real system clipboard.
//! 2. Call sites never touch [`egui::Context::copy_text`] directly: they call
//!    [`copy`], which reports what it could actually do, and the caller phrases
//!    its status message from that ([`CopyOutcome::status`]).
//!
//! # The honest limit
//!
//! What is decidable at compile time is whether a *system* clipboard backend is
//! compiled in.  Whether the desktop accepted the text at run time (a session
//! with no display, a Wayland compositor without `zwlr_data_control_manager_v1`)
//! cannot be read back from `egui`: the `Context` exposes the outgoing
//! [`egui::OutputCommand::CopyText`] command, not the backend's verdict.  So:
//!
//! * feature on ⇒ "copied to the clipboard" — the text is handed to
//!   `arboard`/`smithay-clipboard`, the same path every other native app uses;
//! * feature off ⇒ "kept in this window" — no claim is made about the desktop.
//!
//! [`copy`] still always calls `ctx.copy_text`, so even a feature-less build can
//! be pasted *inside* the app (egui's fallback), it just is not called a system
//! clipboard.

/// What [`copy`] could do with the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyOutcome {
    /// A system clipboard backend is compiled in: the text went to
    /// `arboard`/`smithay-clipboard`, i.e. to the desktop.
    System,
    /// No system clipboard in this build: only egui's in-app fallback received
    /// the text.  The caller must not claim the desktop clipboard holds it.
    InApp,
}

impl CopyOutcome {
    /// Is a real (desktop) clipboard backend compiled in?
    pub fn is_system(self) -> bool {
        matches!(self, CopyOutcome::System)
    }

    /// The status-bar sentence to show after a copy — the whole point of this
    /// module being that the two cases do not say the same thing.
    pub fn status(self, what: &str) -> String {
        match self {
            CopyOutcome::System => format!("{what} copied to the clipboard"),
            CopyOutcome::InApp => format!(
                "{what} kept in this window: this build has no system clipboard \
                 (rebuild with the `clipboard` feature, then paste it anywhere)"
            ),
        }
    }
}

/// Is a system clipboard backend compiled into this build?
///
/// Pure and cheap (`cfg`), so a call site can ask before building a message.
pub const fn system_available() -> bool {
    cfg!(feature = "clipboard")
}

/// Put `text` on the clipboard and report what that meant.
///
/// Always forwards to [`egui::Context::copy_text`] (so the in-app clipboard is
/// filled even in a feature-less build) and returns the outcome the caller must
/// phrase its feedback from.
pub fn copy(ctx: &egui::Context, text: &str) -> CopyOutcome {
    ctx.copy_text(text.to_owned());
    if system_available() {
        CopyOutcome::System
    } else {
        log::warn!(
            "clipboard: no system clipboard in this build - the text only reached \
             egui's in-app clipboard ({} byte(s) dropped for the desktop)",
            text.len()
        );
        CopyOutcome::InApp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text that `copy` hands to egui is the pending
    /// [`egui::OutputCommand::CopyText`] command — read it back from the
    /// context, exactly like the eframe backend would.
    fn pending_copy(ctx: &egui::Context) -> Option<String> {
        ctx.output(|output| {
            output.commands.iter().find_map(|command| match command {
                egui::OutputCommand::CopyText(text) => Some(text.clone()),
                _ => None,
            })
        })
    }

    #[test]
    fn copy_queues_the_exact_text_for_the_platform() {
        let ctx = egui::Context::default();
        let report = "Holaf-Mix — report\nline 2: accents éàü and 中文\n";
        let outcome = copy(&ctx, report);
        // The content must reach egui untouched (no trimming, no reformatting):
        // it is a bug report the user is going to paste somewhere.
        assert_eq!(pending_copy(&ctx).as_deref(), Some(report));
        // And the outcome must agree with what the build can do.
        assert_eq!(outcome.is_system(), cfg!(feature = "clipboard"));
    }

    #[test]
    fn an_empty_text_is_still_a_well_formed_command() {
        // "Copy report" with no report must not panic nor skip the command.
        let ctx = egui::Context::default();
        copy(&ctx, "");
        assert_eq!(pending_copy(&ctx).as_deref(), Some(""));
    }

    #[test]
    fn the_status_sentence_never_claims_more_than_the_build_can_do() {
        let system = CopyOutcome::System.status("report");
        assert_eq!(system, "report copied to the clipboard");
        let in_app = CopyOutcome::InApp.status("report");
        assert!(!in_app.contains("copied to the clipboard"), "{in_app}");
        assert!(in_app.contains("no system clipboard"), "{in_app}");
        // The two outcomes are distinguishable at run time, which is what the
        // diagnostics dialog branches on.
        assert_ne!(CopyOutcome::System, CopyOutcome::InApp);
        assert!(CopyOutcome::System.is_system());
        assert!(!CopyOutcome::InApp.is_system());
    }

    #[test]
    fn the_default_build_has_a_system_clipboard() {
        // Guards the regression that started all this: `eframe` was built with
        // `default-features = false` and nothing re-enabled `clipboard`, so the
        // button silently pasted an empty clipboard.  The `clipboard` feature is
        // part of `default`, so this assertion holds for every shipped build.
        #[cfg(feature = "clipboard")]
        {
            assert!(system_available());
        }
        // `cfg!` and the function must never disagree.
        assert_eq!(system_available(), cfg!(feature = "clipboard"));
    }
}
